use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use fdm_core::{paths, SettingsStore};
use md5::{Digest, Md5};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ed2kState {
    Downloading,
    Waiting,
    Hashing,
    Paused,
    Completed,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct Ed2kPart {
    pub state: String,
    pub sources: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Ed2kStats {
    pub state: Option<Ed2kState>,
    pub name: Option<String>,
    pub size: u64,
    pub completed: u64,
    pub speed: u64,
    pub sources: usize,
    pub uploaded: u64,
    pub upload_speed: u64,
    pub parts: Vec<Ed2kPart>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Ed2kStatus {
    pub ready: bool,
    pub server_connected: bool,
    pub kad_connected: bool,
    pub low_id: bool,
    pub server_name: Option<String>,
}

/// aMule 3.1.0 本地引擎：负责 amuled/amuleapi 生命周期与 REST 调用。
pub struct Ed2kEngine {
    client: Client,
    api_base: String,
    token: RwLock<Option<String>>,
    password: String,
    config_dir: PathBuf,
    download_dir: StdRwLock<PathBuf>,
    children: Mutex<Vec<Child>>,
    hashes: RwLock<HashMap<i64, String>>,
    settings: Arc<SettingsStore>,
    #[cfg(windows)]
    job: AtomicUsize,
}

/// 判断文件是否需要刷新（不存在或修改时间超过 TTL）。
fn should_refresh(path: &Path, ttl: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    if meta.len() == 0 {
        return true;
    }
    let Ok(modified) = meta.modified() else {
        return true;
    };
    std::time::SystemTime::now()
        .duration_since(modified)
        .map(|age| age > ttl)
        .unwrap_or(true)
}

/// 后台从多源回退下载并校验二进制列表（如 nodes.dat / server.met）。
async fn download_and_verify_binary(
    client: &Client,
    urls: &[String],
    target_path: &Path,
    min_bytes: usize,
) -> bool {
    for url in urls {
        match client.get(url).timeout(Duration::from_secs(10)).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes().await {
                    if bytes.len() >= min_bytes
                        && !bytes.starts_with(b"<!DOCTYPE")
                        && !bytes.starts_with(b"<html")
                        && !bytes.starts_with(b"<HTML")
                    {
                        if let Ok(()) = paths::atomic_write(target_path, &bytes) {
                            tracing::info!(path = ?target_path, url = %url, bytes = bytes.len(), "ED2K 节点列表更新成功");
                            return true;
                        }
                    }
                }
            }
            Ok(resp) => {
                tracing::debug!(url = %url, status = %resp.status(), "ED2K 源响应非成功状态");
            }
            Err(err) => {
                tracing::debug!(url = %url, error = %err, "ED2K 源请求失败，尝试下一镜像");
            }
        }
    }
    false
}

impl Ed2kEngine {
    pub async fn new(
        data_dir: &Path,
        settings: Arc<SettingsStore>,
        amule_dir: PathBuf,
    ) -> Result<Arc<Self>> {
        let ed2k = settings.get().ed2k;
        if !ed2k.enabled {
            return Err(anyhow!("ED2K 已禁用"));
        }
        let config_dir = data_dir.join("engine").join("amule");
        paths::ensure_dir(&config_dir)?;
        let download_dir = PathBuf::from(&settings.get().default_dir);
        paths::ensure_dir(&download_dir)?;
        paths::ensure_dir(&download_dir.join(".fdm-temp"))?;
        paths::ensure_dir(&download_dir.join(".fdm-ed2k"))?;

        let amuled = find_binary(&amule_dir, "amuled")?;
        let amuleapi = find_binary(&amule_dir, "amuleapi")?;

        let secret_file = config_dir.join("api-secret");
        let password = match std::fs::read_to_string(&secret_file) {
            Ok(value) if value.trim().len() >= 16 => value.trim().to_string(),
            _ => {
                let value = random_password();
                paths::atomic_write(&secret_file, value.as_bytes())?;
                value
            }
        };

        write_amule_config(&config_dir, &download_dir, &ed2k, &password)?;
        write_api_config(&config_dir, &ed2k, &password)?;

        cleanup_stale(&config_dir);
        // 每次启动都同步 amuleapi 的管理员密码，避免 api-secret 与密码文件不一致。
        let cfg = config_dir.to_string_lossy().to_string();
        let pass_arg = format!("--set-admin-pass={password}");
        run_once(&amuleapi, &["--config-dir", &cfg, &pass_arg])?;

        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .context("创建 aMule API 客户端失败")?;
        let api_base = format!("http://127.0.0.1:{}", ed2k.api_port);

        let engine = Arc::new(Self {
            client,
            api_base,
            token: RwLock::new(None),
            password,
            config_dir: config_dir.clone(),
            download_dir: StdRwLock::new(download_dir),
            children: Mutex::new(Vec::new()),
            hashes: RwLock::new(HashMap::new()),
            settings: settings.clone(),
            #[cfg(windows)]
            job: AtomicUsize::new(create_kill_on_close_job()),
        });

        engine.spawn_daemons(&amuled, &amuleapi, &ed2k).await?;
        if let Err(err) = engine.wait_ready().await {
            // aMule 3.1.0 解析被自动合并坏的 server.met 时会触发断言并退出。
            // 删除本地服务器列表后重试一次；amuleapi 会从 serverMetUrl 重新下载干净列表。
            let server_met = config_dir.join("server.met");
            let server_bak = config_dir.join("server.met.bak");
            let had_list = server_met.exists() || server_bak.exists();
            if !had_list {
                return Err(err);
            }
            tracing::warn!(error = %err, "ED2K 首次启动失败，清理 server.met 后重试");
            let _ = std::fs::remove_file(&server_met);
            let _ = std::fs::remove_file(&server_bak);
            let _ = std::fs::remove_file(config_dir.join("server.met.download"));
            engine.spawn_daemons(&amuled, &amuleapi, &ed2k).await?;
            engine.wait_ready().await?;
        }
        engine.login().await?;
        // 稀有文件（长尾 ISO）保留已知 source，重启后不必从零再找。
        let _ = engine
            .request(
                Method::PATCH,
                "/api/v1/preferences",
                Some(json!({"files": {"save_sources_for_rare_files": true}})),
            )
            .await;

                let engine_bg = engine.clone();
        let server_url = ed2k.server_met_url.clone();
        let nodes_url = ed2k.nodes_dat_url.clone();
        let config_dir_bg = config_dir.clone();
        tokio::spawn(async move {
            let http = Client::builder()
                .user_agent(fdm_core::USER_AGENT)
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default();
            let ttl = Duration::from_secs(24 * 3600);

            // 1. 同步 nodes.dat (Kad 引导节点)
            let nodes_file = config_dir_bg.join("nodes.dat");
            let nodes_synced = if should_refresh(&nodes_file, ttl) {
                let mut candidates = vec![
                    nodes_url.clone(),
                    "https://upd.emule-security.org/nodes.dat".to_string(),
                    "https://nodes-dat.com/nodes.dat".to_string(),
                    "http://www.kademlia.ru/download/nodes.dat".to_string(),
                ];
                for proxy in fdm_core::GITHUB_PROXY_PRESETS {
                    candidates.push(format!(
                        "{proxy}https://raw.githubusercontent.com/alantany/emule-nodes/master/nodes.dat"
                    ));
                }
                candidates.retain(|u| !u.trim().is_empty());
                download_and_verify_binary(&http, &candidates, &nodes_file, 512).await
            } else {
                true
            };

            // 2. 同步 server.met (ED2K 服务器列表)
            let server_file = config_dir_bg.join("server.met");
            let server_synced = if should_refresh(&server_file, ttl) {
                let mut candidates = vec![
                    server_url.clone(),
                    "https://upd.emule-security.org/server.met".to_string(),
                    "http://www.gruk.org/server.met".to_string(),
                    "http://www.peerdates.com/servers.met".to_string(),
                ];
                candidates.retain(|u| !u.trim().is_empty());
                download_and_verify_binary(&http, &candidates, &server_file, 128).await
            } else {
                true
            };

            // 仅在 Rust 端多源更新失败时，作为回退让 amuled 原生重试远端 URL
            if !server_synced && !server_url.trim().is_empty() {
                let _ = engine_bg
                    .request(
                        Method::POST,
                        "/api/v1/servers_update",
                        Some(json!({"url": server_url})),
                    )
                    .await;
            }
            if !nodes_synced && !nodes_url.trim().is_empty() {
                let _ = engine_bg
                    .request(
                        Method::POST,
                        "/api/v1/kad/update",
                        Some(json!({"url": nodes_url})),
                    )
                    .await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = engine_bg
                .request(
                    Method::POST,
                    "/api/v1/networks/connect",
                    Some(json!({"network":"both"})),
                )
                .await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = engine_bg
                .request(
                    Method::POST,
                    "/api/v1/networks/connect",
                    Some(json!({"network":"both"})),
                )
                .await;
        });

        Ok(engine)
    }

    async fn spawn_daemons(
        &self,
        amuled: &Path,
        amuleapi: &Path,
        ed2k: &fdm_core::Ed2kSettings,
    ) -> Result<()> {
        cleanup_stale(&self.config_dir);
        // kill_children 会关闭 Job Object；重试启动前需要重新创建一个。
        #[cfg(windows)]
        if self.job.load(Ordering::SeqCst) == 0 {
            self.job.store(create_kill_on_close_job(), Ordering::SeqCst);
        }
        let cfg = self.config_dir.to_string_lossy().to_string();
        let ec_port = ed2k.ec_port.to_string();
        let api_port = ed2k.api_port.to_string();
        let mut children = self.children.lock().await;
        let daemon = spawn_hidden(amuled, &["--full-daemon", "--config-dir", &cfg])?;
        let _ = std::fs::write(
            self.config_dir.join("fdm-amuled.pid"),
            daemon.id().to_string(),
        );
        #[cfg(windows)]
        assign_child_to_job(self.job.load(Ordering::SeqCst), daemon.id());
        children.push(daemon);
        let api = spawn_hidden(
            amuleapi,
            &[
                "--config-dir",
                &cfg,
                "--host",
                "127.0.0.1",
                "--port",
                &ec_port,
                "--http-port",
                &api_port,
            ],
        )?;
        let _ = std::fs::write(
            self.config_dir.join("fdm-amuleapi.pid"),
            api.id().to_string(),
        );
        #[cfg(windows)]
        assign_child_to_job(self.job.load(Ordering::SeqCst), api.id());
        children.push(api);
        Ok(())
    }

    async fn wait_ready(&self) -> Result<()> {
        for _ in 0..80 {
            if self.health().await.is_ok() {
                return Ok(());
            }
            // amuled 因 server.met 断言崩溃时，不必等满 24 秒；
            // Windows 下直接感知子进程退出，尽快触发清理重试。
            #[cfg(windows)]
            if self.daemon_exited().await {
                self.kill_children().await;
                bail!("ED2K 守护进程已退出");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.kill_children().await;
        bail!("ED2K 引擎启动超时（amuled/amuleapi 未就绪）")
    }

    #[cfg(windows)]
    async fn daemon_exited(&self) -> bool {
        let mut children = self.children.lock().await;
        children
            .iter_mut()
            .any(|child| matches!(child.try_wait(), Ok(Some(_)) | Err(_)))
    }

    async fn health(&self) -> Result<()> {
        let resp = self
            .client
            .get(format!("{}/api/v1/health", self.api_base))
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            bail!("health {}", resp.status())
        }
    }

    async fn login(&self) -> Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/api/v1/auth/login?include_token=true",
                self.api_base
            ))
            .json(&json!({ "password": self.password }))
            .send()
            .await
            .context("连接 aMule API 失败")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("aMule 登录失败 {}: {}", status.as_u16(), text);
        }
        let value: Value = serde_json::from_str(&text).context("解析 aMule 登录响应失败")?;
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("aMule 登录响应缺少 token"))?
            .to_string();
        *self.token.write().await = Some(token);
        Ok(())
    }

    async fn ensure_token(&self) -> Result<String> {
        if let Some(token) = self.token.read().await.clone() {
            return Ok(token);
        }
        self.login().await?;
        self.token
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("aMule 未登录"))
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..2 {
            let token = self.ensure_token().await?;
            let mut req = self
                .client
                .request(method.clone(), format!("{}{}", self.api_base, path))
                .bearer_auth(token);
            if let Some(payload) = &body {
                req = req.json(payload);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("请求 aMule {path} 失败"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.token.write().await = None;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("aMule API {} {}: {}", status.as_u16(), path, text);
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)));
        }
        bail!("aMule API 认证失败")
    }

    pub async fn add(&self, id: i64, link: &str) -> Result<String> {
        let hash = if let Some(parsed) = fdm_core::parse_ed2k_link(link) {
            parsed.hash
        } else if let Some(magnet) = fdm_core::parse_magnet(link) {
            magnet
                .ed2k_hash
                .ok_or_else(|| anyhow!("无效的 ED2K 链接"))?
        } else {
            return Err(anyhow!("无效的 ED2K 链接"));
        };
        let result = self
            .request(
                Method::POST,
                "/api/v1/downloads",
                Some(json!({ "links": [link], "category_index": 0 })),
            )
            .await?;
        if let Some(items) = result.get("results").and_then(Value::as_array) {
            for item in items {
                if item.get("ok").and_then(Value::as_bool) == Some(false) {
                    let msg = item
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("aMule 拒绝添加");
                    bail!("{}", msg);
                }
            }
        }
        self.hashes.write().await.insert(id, hash.clone());
        Ok(hash)
    }

    /// 重启后把 aMule 队列里已存在的下载重新挂到任务 id 上。
    pub async fn attach(&self, id: i64, hash: &str) -> bool {
        let Ok(value) = self
            .request(Method::GET, "/api/v1/downloads?status=all", None)
            .await
        else {
            return false;
        };
        let found = value
            .get("downloads")
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().any(|item| {
                    item.get("hash")
                        .and_then(Value::as_str)
                        .map(|h| h.eq_ignore_ascii_case(hash))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if found {
            self.hashes
                .write()
                .await
                .insert(id, hash.to_ascii_lowercase());
        }
        found
    }

    pub async fn stats(&self, id: i64) -> Option<Ed2kStats> {
        let hash = self.hashes.read().await.get(&id).cloned()?;
        let value = self
            .request(Method::GET, "/api/v1/downloads?status=all", None)
            .await
            .ok()?;
        let item = value
            .get("downloads")
            .and_then(Value::as_array)?
            .iter()
            .find(|item| {
                item.get("hash")
                    .and_then(Value::as_str)
                    .map(|h| h.eq_ignore_ascii_case(&hash))
                    .unwrap_or(false)
            })?;
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let source_total = item
            .pointer("/sources/total")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let speed = item
            .get("speed_bytes_per_second")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // aMule 会把“已激活但暂无可用源”的文件报成 downloading；这里对外
        // 显示为等待数据源，避免界面长期显示“正在接收数据”却始终 0 B/s。
        let state = match status {
            "downloading" if source_total == 0 => Ed2kState::Waiting,
            "downloading" => Ed2kState::Downloading,
            "waiting" => Ed2kState::Waiting,
            "hashing" | "allocating" | "completing" => Ed2kState::Hashing,
            "paused" | "stopped" => Ed2kState::Paused,
            "completed" => Ed2kState::Completed,
            _ => Ed2kState::Error,
        };
        let parts = self
            .request(Method::GET, &format!("/api/v1/downloads/{hash}"), None)
            .await
            .ok()
            .and_then(|detail| {
                detail
                    .pointer("/progress/parts")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| Ed2kPart {
                                state: item
                                    .get("state")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                                    .to_string(),
                                sources: item.get("sources").and_then(Value::as_u64).unwrap_or(0)
                                    as u32,
                            })
                            .collect()
                    })
            })
            .unwrap_or_default();
        Some(Ed2kStats {
            state: Some(state),
            name: item.get("name").and_then(Value::as_str).map(str::to_string),
            size: item.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
            completed: item
                .get("completed_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            speed,
            sources: source_total as usize,
            uploaded: 0,
            upload_speed: 0,
            parts,
            error: if matches!(state, Ed2kState::Error) {
                Some(format!("aMule 状态：{status}"))
            } else {
                None
            },
        })
    }

    pub async fn pause(&self, id: i64) -> bool {
        self.action(id, "pause").await
    }

    pub async fn resume(&self, id: i64) -> bool {
        self.action(id, "resume").await
    }

    async fn action(&self, id: i64, action: &str) -> bool {
        let Some(hash) = self.hashes.read().await.get(&id).cloned() else {
            return false;
        };
        self.request(
            Method::PATCH,
            &format!("/api/v1/downloads/{hash}"),
            Some(json!({ "action": action })),
        )
        .await
        .is_ok()
    }

    pub async fn remove(&self, id: i64, delete_files: bool, filename: &str) -> bool {
        let Some(hash) = self.hashes.write().await.remove(&id) else {
            return false;
        };
        if delete_files {
            if self
                .request(Method::DELETE, &format!("/api/v1/downloads/{hash}"), None)
                .await
                .is_err()
            {
                let _ = self
                    .request(
                        Method::POST,
                        "/api/v1/downloads_clear_completed",
                        Some(json!({ "hash": hash })),
                    )
                    .await;
            }
            if let Some(path) = self.find_incoming_file(filename) {
                let _ = std::fs::remove_file(path);
            }
        } else {
            let cleared = self
                .request(
                    Method::POST,
                    "/api/v1/downloads_clear_completed",
                    Some(json!({ "hash": hash })),
                )
                .await
                .is_ok();
            if !cleared {
                let _ = self
                    .request(Method::DELETE, &format!("/api/v1/downloads/{hash}"), None)
                    .await;
            }
        }
        true
    }

    pub async fn clear_completed(&self, id: i64) -> bool {
        let Some(hash) = self.hashes.read().await.get(&id).cloned() else {
            return false;
        };
        self.request(
            Method::POST,
            "/api/v1/downloads_clear_completed",
            Some(json!({ "hash": hash })),
        )
        .await
        .is_ok()
    }

    /// 在 Incoming 子目录内定位实际文件。
    ///
    /// aMule 上报的文件名属于远端数据，绝不能直接拼进路径；这里按清洗后的名字
    /// 精确匹配，必要时扫描目录比较清洗结果，杜绝目录穿越。
    fn find_incoming_file(&self, filename: &str) -> Option<PathBuf> {
        let dir = self.download_dir.read().ok()?.clone();
        let incoming = dir.join(".fdm-ed2k");
        let wanted = fdm_core::sanitize_filename(filename);
        let direct = incoming.join(&wanted);
        if direct.is_file() {
            return Some(direct);
        }
        let entries = std::fs::read_dir(&incoming).ok()?;
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if fdm_core::sanitize_filename(&name) == wanted {
                return Some(entry.path());
            }
        }
        None
    }

    /// 完成后把 aMule Incoming 子目录里的文件移动到任务目标路径（跨卷退化为复制）。
    pub fn finalize_file(&self, filename: &str, target: &Path) -> bool {
        let Some(source) = self.find_incoming_file(filename) else {
            return false;
        };
        if source == target {
            return true;
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if target.exists() {
            let _ = std::fs::remove_file(target);
        }
        if std::fs::rename(&source, target).is_ok() {
            return true;
        }
        match std::fs::copy(&source, target) {
            Ok(_) => {
                let _ = std::fs::remove_file(&source);
                true
            }
            Err(_) => false,
        }
    }

    pub async fn status(&self) -> Ed2kStatus {
        let value = self.request(Method::GET, "/api/v1/status", None).await;
        let Ok(value) = value else {
            return Ed2kStatus::default();
        };
        let ed2k = value.get("ed2k");
        let kad = value.get("kad");
        Ed2kStatus {
            ready: true,
            server_connected: ed2k
                .and_then(|v| v.get("state"))
                .and_then(Value::as_str)
                .map(|s| s == "connected")
                .unwrap_or(false),
            kad_connected: kad
                .and_then(|v| v.get("state"))
                .and_then(Value::as_str)
                .map(|s| s == "connected")
                .unwrap_or(false),
            low_id: {
                let connected = ed2k
                    .and_then(|v| v.get("state"))
                    .and_then(Value::as_str)
                    .map(|s| s == "connected")
                    .unwrap_or(false);
                let high = ed2k
                    .and_then(|v| v.get("high_id"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                connected && !high
            },
            server_name: ed2k
                .and_then(|v| v.get("server_name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    pub async fn reconnect(&self) -> bool {
        let ed2k = self.settings.get().ed2k;
        let ok = self
            .request(
                Method::POST,
                "/api/v1/networks/connect",
                Some(json!({"network":"both"})),
            )
            .await
            .is_ok();
        if !ed2k.server_met_url.trim().is_empty() {
            let _ = self
                .request(
                    Method::POST,
                    "/api/v1/servers_update",
                    Some(json!({"url": ed2k.server_met_url})),
                )
                .await;
        }
        if !ed2k.nodes_dat_url.trim().is_empty() {
            let _ = self
                .request(
                    Method::POST,
                    "/api/v1/kad/update",
                    Some(json!({"url": ed2k.nodes_dat_url})),
                )
                .await;
        }
        ok
    }

    pub async fn set_global_rate(&self, kb_per_sec: u64) {
        let _ = self
            .request(
                Method::PATCH,
                "/api/v1/preferences",
                Some(json!({"connection": {"max_download_kibibytes_per_second": kb_per_sec}})),
            )
            .await;
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir
            .read()
            .map(|d| d.clone())
            .unwrap_or_else(|_| PathBuf::new())
    }

    /// 跟随全局默认下载目录调整 aMule Incoming 目录（由 FDM 在无活动 ED2K 任务时调用）。
    pub async fn set_download_dir(&self, dir: &Path) -> bool {
        paths::ensure_dir(dir).is_ok()
            && paths::ensure_dir(&dir.join(".fdm-temp")).is_ok()
            && paths::ensure_dir(&dir.join(".fdm-ed2k")).is_ok()
            && self
                .request(
                    Method::PATCH,
                    "/api/v1/preferences",
                    Some(json!({"directories": {
                        "incoming_path": dir.join(".fdm-ed2k").to_string_lossy(),
                        "temp_path": dir.join(".fdm-temp").to_string_lossy(),
                    }})),
                )
                .await
                .is_ok()
            && self
                .download_dir
                .write()
                .map(|mut current| {
                    *current = dir.to_path_buf();
                })
                .is_ok()
    }

    pub async fn shutdown(&self) {
        self.kill_children().await;
    }

    async fn kill_children(&self) {
        let mut children = self.children.lock().await;
        for child in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        children.clear();
        let _ = std::fs::remove_file(self.config_dir.join("fdm-amuled.pid"));
        let _ = std::fs::remove_file(self.config_dir.join("fdm-amuleapi.pid"));
        #[cfg(windows)]
        {
            let handle = self.job.swap(0, Ordering::SeqCst);
            if handle != 0 {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(
                        handle as windows_sys::Win32::Foundation::HANDLE,
                    );
                }
            }
        }
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> usize {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            windows_sys::Win32::Foundation::CloseHandle(job);
            return 0;
        }
        job as usize
    }
}

#[cfg(windows)]
fn assign_child_to_job(job: usize, pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };
    if job == 0 {
        return;
    }
    unsafe {
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return;
        }
        let _ = AssignProcessToJobObject(job as HANDLE, process);
        CloseHandle(process);
    }
}

fn find_binary(dir: &Path, name: &str) -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let candidates = [dir.join(&exe), dir.join("bin").join(&exe)];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("未找到 {}（{}）", exe, dir.display()))
}

fn run_once(exe: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let status = command
        .status()
        .with_context(|| format!("执行 {} 失败", exe.display()))?;
    if !status.success() {
        bail!("{} 退出码 {:?}", exe.display(), status.code());
    }
    Ok(())
}

fn spawn_hidden(exe: &Path, args: &[&str]) -> Result<Child> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .with_context(|| format!("启动 {} 失败", exe.display()))
}

fn cleanup_stale(config_dir: &Path) {
    for name in ["fdm-amuled.pid", "fdm-amuleapi.pid"] {
        let path = config_dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let pid = text.trim();
        if pid.is_empty() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", pid, "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill").args(["-9", pid]).status();
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn md5_hex(value: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn random_password() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut out = String::with_capacity(32);
    for salt in 0u8..4 {
        let mut hasher = RandomState::new().build_hasher();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        salt.hash(&mut hasher);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

fn write_amule_config(
    config_dir: &Path,
    download_dir: &Path,
    ed2k: &fdm_core::Ed2kSettings,
    password: &str,
) -> Result<()> {
    // 临时文件放在 IncomingDir 同一分区的隐藏目录：完成时是同卷 rename，
    // 既不会把 C: 塞满，也不会出现“下载完再整盘复制”的卡顿。
    let temp = download_dir.join(".fdm-temp");
    paths::ensure_dir(&temp)?;
    // aMule 会把 IncomingDir 视为共享目录，绝不能直接指向用户的下载根目录，
    // 否则整个 Downloads 都会被哈希并公开共享。用独立子目录，完成后搬到目标路径。
    let incoming_path = download_dir.join(".fdm-ed2k");
    paths::ensure_dir(&incoming_path)?;
    // aMule 的 wxConfig 会把值里的反斜杠当成转义符（单反斜杠路径会被吃掉，
    // 例如 C:\Users\... 变成 C:sers... 并在 C: 根目录建目录）。统一用正斜杠。
    let incoming = slash_path(&incoming_path);
    let temp_dir = slash_path(&temp);
    // aMule 3.1.0 的 Windows 构建开启协议混淆后无法与 ed2k 源建立连接：
    // 搜索能列出源，但添加任务后始终 0 源、0 B/s。Windows 下改为明文连接，
    // 其它平台暂保持原行为。
    let crypt_layer = if cfg!(windows) { 0 } else { 1 };
    let conf = format!(
        concat!(
            "[eMule]\n",
            "AppVersion=3.1.0\n",
            "Nick=FDM\n",
            "IncomingDir={incoming}\n",
            "TempDir={temp_dir}\n",
            "Port={tcp_port}\n",
            "UDPPort={udp_port}\n",
            "ConnectToED2K=1\n",
            "ConnectToKad=1\n",
            "FirstRunWizardDone=1\n",
            // 单文件 source / 连接上限：aMule 默认 300 / 480，放大后长尾文件也能跑满。
            "MaxDownload={download_limit}\n",
            "MaxUpload=100\n",
            "MaxConnections=1000\n",
            "MaxConnectionsPerFiveSeconds=100\n",
            "MaxSourcesPerFile=1000\n",
            // aMule 允许的最小重问间隔是 15/30 分钟（API 会拒绝更小的值）。
            "SourceReaskMinutes=15\n",
            "KadSourceReaskMinutes=30\n",
            "KadMaxSourceSearches=50\n",
            // 丢弃长期 0 速 source，腾出连接给活 source。
            "DropSlowSources=1\n",
            "Endgame=1\n",
            // 允许从服务器/客户端补全 server.met，长期扩大可用服务器池。
            "AddServerListFromServer=1\n",
            "AddServerListFromClient=1\n",
            "Autoconnect=1\n",
            "Reconnect=1\n",
            "DeadServerRetry=3\n",
            "RemoveDeadServer=1\n",
            "SmartIdCheck=1\n",
            // 大文件不预分配整块空间，避免 C: 空间被瞬间占满（aMule 默认会分配 8GB+）。
            "AllocateFullFile=0\n",
            "CreateSparseFiles=1\n",
            // 路由器支持 UPnP 时自动开 4662 端口，LowID -> HighID 是 ED2K 提速关键。
            "UPnPEnabled=1\n",
            "UPnPTCPPort={tcp_port}\n",
            "UDPEnable=1\n",
            "UDPDisable=0\n",
            "Ed2kServersUrl={server_url}\n",
            "KadNodesUrl={nodes_url}\n",
            "[Obfuscation]\n",
            "IsClientCryptLayerSupported={crypt_layer}\n",
            "IsCryptLayerRequested={crypt_layer}\n",
            "IsClientCryptLayerRequired=0\n",
            "[ExternalConnect]\n",
            "AcceptExternalConnections=1\n",
            "ECPort={ec_port}\n",
            "ECPassword={ec_password}\n",
            "[AmuleApi]\n",
            "Enabled=0\n"
        ),
        incoming = incoming,
        temp_dir = temp_dir,
        tcp_port = ed2k.tcp_port,
        udp_port = ed2k.udp_port,
        download_limit = ed2k.download_limit_kb,
        server_url = ed2k.server_met_url,
        nodes_url = ed2k.nodes_dat_url,
        ec_port = ed2k.ec_port,
        ec_password = md5_hex(password),
        crypt_layer = crypt_layer,
    );
    paths::atomic_write(&config_dir.join("amule.conf"), conf.as_bytes())
        .map_err(|err| anyhow!(err.to_string()))
}

/// 把 Windows 路径转成 aMule 可安全读取的正斜杠形式。
fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn write_api_config(
    config_dir: &Path,
    ed2k: &fdm_core::Ed2kSettings,
    password: &str,
) -> Result<()> {
    let conf = format!(
        "[Server]\nBindAddress=127.0.0.1\nPort={}\n[EC]\nHost=127.0.0.1\nPort={}\nPassword={}\n",
        ed2k.api_port, ed2k.ec_port, password,
    );
    paths::atomic_write(&config_dir.join("amuleapi.conf"), conf.as_bytes())
        .map_err(|err| anyhow!(err.to_string()))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_filename_cannot_escape_incoming_dir() {
        let base = Path::new("C:/Downloads");
        let incoming = base.join(".fdm-ed2k");
        for name in [
            "..\\..\\evil.exe",
            "../../evil.exe",
            "C:\\Windows\\evil.exe",
            "a/b/c.bin",
        ] {
            let joined = incoming.join(fdm_core::sanitize_filename(name));
            assert!(
                joined.starts_with(&incoming),
                "{name} -> {}",
                joined.display()
            );
            assert!(
                !joined
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "{name} -> {}",
                joined.display()
            );
        }
    }
}
