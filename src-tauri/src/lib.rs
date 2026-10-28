use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fdm_core::protocol::detect_kind;
use fdm_core::{github_proxy_candidates, paths, Settings, SettingsStore};
use fdm_engine::{EnginePaths, TaskManager};
use fdm_platform::{PlatformService, SystemPlatform};
use fdm_server::{
    ensure_extension_assets, serve, DesktopHooks, ServerState, UpdateInfo, UpdateStatus,
};
use serde_json::{json, Value};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_updater::{Updater, UpdaterExt};
use url::Url;

const BASE_URL: &str = "http://127.0.0.1:25432";
const UPDATE_ENDPOINT: &str =
    "https://github.com/ZHDeveloper/fdm-rust/releases/latest/download/latest.json";

pub struct AppRuntime {
    pub hooks: Arc<TauriHooks>,
    pub manager: Arc<TaskManager>,
    pub quitting: Arc<AtomicBool>,
}

pub struct TauriHooks {
    app: AppHandle,
    manager: Arc<TaskManager>,
    settings: Arc<SettingsStore>,
    quitting: Arc<AtomicBool>,
    update_status: Arc<Mutex<UpdateStatus>>,
    update_busy: Arc<AtomicBool>,
}

impl TauriHooks {
    fn new(
        app: AppHandle,
        manager: Arc<TaskManager>,
        settings: Arc<SettingsStore>,
        quitting: Arc<AtomicBool>,
    ) -> Self {
        Self {
            app,
            manager,
            settings,
            quitting,
            update_status: Arc::new(Mutex::new(UpdateStatus::default())),
            update_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn current_version(&self) -> String {
        app_version(&self.app)
    }

    fn publish_status(&self, status: UpdateStatus) {
        publish_update_status(&self.manager, &self.update_status, status);
    }

    fn set_update_error(&self, message: String) {
        let mut status = self
            .update_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        status.phase = "error".to_string();
        status.current_version = self.current_version();
        status.error = Some(message);
        self.publish_status(status);
    }

    fn show_existing(&self, label: &str) -> bool {
        if let Some(window) = self.app.get_webview_window(label) {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
            true
        } else {
            false
        }
    }

    fn create_window(
        &self,
        label: &str,
        url: String,
        title: &str,
        width: f64,
        height: f64,
        resizable: bool,
    ) -> Result<bool> {
        let parsed = Url::parse(&url)?;
        let mut builder = WebviewWindowBuilder::new(&self.app, label, WebviewUrl::External(parsed))
            .title(title)
            .inner_size(width, height)
            .resizable(resizable)
            .center();
        if label.starts_with("prog_") {
            builder = builder.min_inner_size(460.0, 280.0);
        }
        let window = builder.build()?;
        let _ = window.set_focus();
        Ok(true)
    }
}

#[async_trait]
impl DesktopHooks for TauriHooks {
    async fn open_progress(&self, task_id: i64) -> Result<bool> {
        let label = format!("prog_{}", task_id);
        if self.show_existing(&label) {
            return Ok(true);
        }
        let title = self
            .manager
            .get_task(task_id)
            .await
            .map(|t| format!("{} - 下载进度", t.filename))
            .unwrap_or_else(|| format!("任务 #{} - 下载进度", task_id));
        let url = format!("{}/progress.html?taskId={}", BASE_URL, task_id);
        self.create_window(&label, url, &title, 540.0, 360.0, true)
    }

    async fn open_settings(&self) -> Result<bool> {
        if self.show_existing("settings") {
            return Ok(true);
        }
        self.create_window(
            "settings",
            format!("{}/settings.html", BASE_URL),
            "FDM - 设置选项",
            520.0,
            400.0,
            false,
        )
    }

    async fn open_extension_guide(&self) -> Result<bool> {
        if self.show_existing("extension_guide") {
            return Ok(true);
        }
        let opened = self.create_window(
            "extension_guide",
            format!("{}/extension-guide.html", BASE_URL),
            "FDM - 安装浏览器扩展",
            560.0,
            400.0,
            false,
        )?;
        if opened {
            if let Err(err) = self
                .settings
                .update_patch(&json!({"extensionGuideShown": true}))
            {
                tracing::warn!(error = %err, "记录浏览器扩展引导展示状态失败");
            }
        }
        Ok(opened)
    }

    async fn open_main(&self) -> Result<bool> {
        Ok(self.show_existing("main"))
    }

    async fn open_new_download(&self, prefill: Value) -> Result<bool> {
        let mut url = Url::parse(&format!("{}/newdownload.html", BASE_URL))?;
        if let Some(obj) = prefill.as_object() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in obj {
                if value.is_null() {
                    continue;
                }
                let text = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                pairs.append_pair(key, &text);
            }
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis().to_string())
                .unwrap_or_else(|_| "0".to_string());
            pairs.append_pair("reopen_ts", &ts);
        }
        if let Some(window) = self.app.get_webview_window("newdownload") {
            let js = format!(
                "window.location.href = {};",
                serde_json::to_string(url.as_str())?
            );
            let _ = window.eval(&js);
            let _ = window.set_size(tauri::Size::Logical(LogicalSize::new(520.0, 189.0)));
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
            return Ok(true);
        }
        self.create_window(
            "newdownload",
            url.to_string(),
            "下载文件信息",
            520.0,
            189.0,
            false,
        )
    }

    async fn open_duplicate_dialog(&self, id: &str, title: &str) -> Result<bool> {
        let mut url = Url::parse(&format!("{}/duplicate.html", BASE_URL))?;
        url.query_pairs_mut().append_pair("id", id);
        if let Some(window) = self.app.get_webview_window("duplicate") {
            let js = format!(
                "window.location.href = {};",
                serde_json::to_string(url.as_str())?
            );
            let _ = window.eval(&js);
            let _ = window.set_title(title);
            let _ = window.set_size(tauri::Size::Logical(LogicalSize::new(410.0, 185.0)));
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
            return Ok(true);
        }
        self.create_window("duplicate", url.to_string(), title, 410.0, 185.0, false)
    }

    async fn close_progress(&self, task_id: i64) -> Result<bool> {
        let label = format!("prog_{}", task_id);
        if let Some(window) = self.app.get_webview_window(&label) {
            let _ = window.close();
            return Ok(true);
        }
        Ok(false)
    }

    async fn app_version(&self) -> Result<String> {
        Ok(self.current_version())
    }

    async fn update_status(&self) -> Result<UpdateStatus> {
        Ok(self
            .update_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default())
    }

    async fn check_update(&self) -> Result<Option<UpdateInfo>> {
        if self.update_busy.load(Ordering::SeqCst) {
            bail!("更新正在下载或安装");
        }
        let mut checking = UpdateStatus::default();
        checking.phase = "checking".to_string();
        checking.current_version = self.current_version();
        self.publish_status(checking);

        let updater = match build_updater(&self.app, &self.settings.get()) {
            Ok(updater) => updater,
            Err(err) => {
                self.set_update_error(err.to_string());
                return Err(err);
            }
        };
        match updater.check().await {
            Ok(Some(update)) => {
                let info = UpdateInfo {
                    version: update.version.clone(),
                    current_version: update.current_version.clone(),
                    notes: update.body.clone(),
                    pub_date: update.date.map(|date| date.to_string()),
                };
                let mut status = UpdateStatus::default();
                status.phase = "available".to_string();
                status.current_version = info.current_version.clone();
                status.version = Some(info.version.clone());
                status.notes = info.notes.clone();
                self.publish_status(status);
                self.manager.broadcast(
                    "update:available",
                    serde_json::to_value(&info).unwrap_or(Value::Null),
                );
                Ok(Some(info))
            }
            Ok(None) => {
                let mut status = UpdateStatus::default();
                status.phase = "up_to_date".to_string();
                status.current_version = self.current_version();
                self.publish_status(status);
                Ok(None)
            }
            Err(err) => {
                self.set_update_error(err.to_string());
                Err(err.into())
            }
        }
    }

    async fn install_update(&self, expected_version: &str) -> Result<()> {
        if self
            .update_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("已有更新任务正在执行");
        }

        let app = self.app.clone();
        let manager = self.manager.clone();
        let settings = self.settings.clone();
        let status_slot = self.update_status.clone();
        let busy = self.update_busy.clone();
        let expected_version = expected_version.to_string();

        tauri::async_runtime::spawn(async move {
            let result: Result<()> = async {
                let updater = build_updater(&app, &settings.get())?;
                let mut update = match updater.check().await? {
                    Some(update) => update,
                    None => bail!("没有可用更新"),
                };
                if update.version != expected_version {
                    bail!(
                        "更新版本已变化：期望 {}，实际 {}",
                        expected_version,
                        update.version
                    );
                }

                let update_version = update.version.clone();
                let update_notes = update.body.clone();
                let candidates =
                    github_proxy_candidates(update.download_url.as_str(), &settings.get(), false);
                let mut last_error: Option<String> = None;
                let mut installed = false;
                for candidate in candidates {
                    let Ok(url) = Url::parse(&candidate) else {
                        continue;
                    };
                    update.download_url = url;

                    let mut status = UpdateStatus::default();
                    status.phase = "downloading".to_string();
                    status.current_version = app_version(&app);
                    status.version = Some(update_version.clone());
                    status.notes = update_notes.clone();
                    publish_update_status(&manager, &status_slot, status);

                    let mut downloaded = 0u64;
                    let mut last_emit = Instant::now() - Duration::from_secs(1);
                    let manager_for_progress = manager.clone();
                    let status_for_progress = status_slot.clone();
                    let app_for_progress = app.clone();
                    let app_for_install = app.clone();
                    let version_for_progress = update_version.clone();
                    let notes_for_progress = update_notes.clone();
                    let version_for_install = update_version.clone();
                    let notes_for_install = update_notes.clone();
                    match update
                        .download_and_install(
                            move |chunk, total| {
                                downloaded += chunk as u64;
                                if last_emit.elapsed() < Duration::from_millis(250) {
                                    return;
                                }
                                last_emit = Instant::now();
                                let mut progress = UpdateStatus::default();
                                progress.phase = "downloading".to_string();
                                progress.current_version = app_version(&app_for_progress);
                                progress.version = Some(version_for_progress.clone());
                                progress.notes = notes_for_progress.clone();
                                progress.downloaded = downloaded;
                                progress.total = total;
                                publish_update_status(
                                    &manager_for_progress,
                                    &status_for_progress,
                                    progress,
                                );
                            },
                            {
                                let manager = manager.clone();
                                let status_slot = status_slot.clone();
                                let version = version_for_install.clone();
                                let notes = notes_for_install.clone();
                                move || {
                                    let mut status = UpdateStatus::default();
                                    status.phase = "installing".to_string();
                                    status.current_version = app_version(&app_for_install);
                                    status.version = Some(version);
                                    status.notes = notes;
                                    publish_update_status(&manager, &status_slot, status);
                                }
                            },
                        )
                        .await
                    {
                        Ok(()) => {
                            installed = true;
                            break;
                        }
                        Err(err) => last_error = Some(err.to_string()),
                    }
                }
                if !installed {
                    bail!(
                        "更新包下载失败：{}",
                        last_error.unwrap_or_else(|| "没有可用下载线路".to_string())
                    );
                }

                let mut status = UpdateStatus::default();
                status.phase = "installing".to_string();
                status.current_version = app_version(&app);
                status.version = Some(update_version);
                status.notes = update_notes;
                publish_update_status(&manager, &status_slot, status);

                let _ = manager.save_tasks().await;
                manager.shutdown_engines().await;

                #[cfg(target_os = "macos")]
                {
                    if let Err(err) = restart_macos(&app) {
                        tracing::error!(error = %err, "准备 macOS 重启脚本失败，回退到 app.restart()");
                        app.restart();
                    } else {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        std::process::exit(0);
                    }
                }

                #[cfg(all(not(windows), not(target_os = "macos")))]
                app.restart();

                #[cfg(windows)]
                let _ = &app;

                Ok(())
            }
            .await;

            if let Err(err) = result {
                let mut status = status_slot.lock().map(|s| s.clone()).unwrap_or_default();
                status.phase = "error".to_string();
                status.current_version = app_version(&app);
                status.error = Some(err.to_string());
                publish_update_status(&manager, &status_slot, status);
            }
            busy.store(false, Ordering::SeqCst);
        });

        Ok(())
    }

    async fn window_control(&self, action: &str, target: Option<String>, params: Value) -> Value {
        let label = target.unwrap_or_else(|| "main".to_string());
        let Some(window) = self.app.get_webview_window(&label) else {
            return json!({"ok": false, "error": format!("窗口不存在: {}", label)});
        };
        match action {
            "minimize" => json!({"ok": window.minimize().is_ok()}),
            "maximize" => {
                let maximized = window.is_maximized().unwrap_or(false);
                let ok = if maximized {
                    window.unmaximize().is_ok()
                } else {
                    window.maximize().is_ok()
                };
                json!({"ok": ok})
            }
            "resize" => {
                let width = params.get("width").and_then(Value::as_f64).unwrap_or(720.0);
                let height = params
                    .get("height")
                    .and_then(Value::as_f64)
                    .unwrap_or(420.0);
                let ok = window
                    .set_size(tauri::Size::Logical(LogicalSize::new(width, height)))
                    .is_ok();
                json!({"ok": ok, "width": width, "height": height})
            }
            "close" => {
                let ok = if label == "main" {
                    window.hide().is_ok()
                } else {
                    window.close().is_ok()
                };
                json!({"ok": ok})
            }
            other => json!({"ok": false, "error": format!("未知窗口操作: {}", other)}),
        }
    }

    async fn quit(&self) -> Result<()> {
        let _ = self.manager.save_tasks().await;
        self.manager.shutdown_engines().await;
        self.quitting.store(true, Ordering::SeqCst);
        self.app.exit(0);
        Ok(())
    }
}

#[cfg(not(debug_assertions))]
fn spawn_startup_update_check(hooks: Arc<TauriHooks>, platform: Arc<dyn PlatformService>) {
    if std::env::var_os("FDM_DISABLE_UPDATE_CHECK").is_some() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        match hooks.check_update().await {
            Ok(Some(info)) => {
                let version = info.version.clone();
                let message = format!(
                    "发现 FDM 新版本 v{}（当前 v{}）。\n\n是否立即下载并安装？\n\n更新内容可在“设置 → 关于”查看。",
                    info.version, info.current_version
                );
                let confirmed = tokio::task::spawn_blocking(move || {
                    platform.message_box("FDM 更新", &message, "question", true)
                })
                .await
                .unwrap_or(false);
                if confirmed {
                    if let Err(err) = hooks.install_update(&version).await {
                        tracing::warn!(error = %err, "启动更新安装失败");
                    }
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "启动更新检查失败"),
        }
    });
}

fn app_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

fn encode_proxy_part(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn updater_proxy(settings: &Settings) -> Option<Url> {
    if !settings.proxy_enabled {
        return None;
    }
    let host = settings.proxy_host.trim();
    if host.is_empty() {
        return None;
    }
    let scheme = match settings.proxy_type.as_str() {
        "https" => "https",
        "socks5" => "socks5h",
        "socks4" => "socks4",
        _ => "http",
    };
    let mut authority = host.to_string();
    if !settings.proxy_port.trim().is_empty() {
        authority.push(':');
        authority.push_str(settings.proxy_port.trim());
    }
    let auth = if settings.proxy_user.trim().is_empty() {
        String::new()
    } else {
        format!(
            "{}:{}@",
            encode_proxy_part(settings.proxy_user.trim()),
            encode_proxy_part(&settings.proxy_pass)
        )
    };
    Url::parse(&format!("{scheme}://{auth}{authority}")).ok()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn find_bundle_path(exe: &std::path::Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| p.extension().and_then(|ext| ext.to_str()) == Some("app"))
        .map(|p| p.to_path_buf())
}

#[cfg(target_os = "macos")]
fn restart_macos(app: &AppHandle) -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let bundle_path = find_bundle_path(&current_exe);

    let pid = std::process::id();
    let identifier = app.config().identifier.replace(['.', '-'].as_ref(), "_");
    let socket_path = format!("/tmp/{}_si.sock", identifier);

    let (target_path, is_bundle) = if let Some(bundle) = bundle_path {
        (bundle, "1")
    } else {
        (current_exe, "0")
    };

    // shortcut: 最多等待5秒旧进程消亡并清理单例socket，避免新实例被杀或端口冲突
    let script = r#"
        pid="$1"
        sock="$2"
        target="$3"
        is_bundle="$4"
        count=0
        while kill -0 "$pid" 2>/dev/null && [ "$count" -lt 50 ]; do
            sleep 0.1
            count=$((count + 1))
        done
        rm -f "$sock" 2>/dev/null
        if [ "$is_bundle" = "1" ]; then
            /usr/bin/open -n "$target"
        else
            "$target"
        fi
    "#;

    use std::os::unix::process::CommandExt;
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("_")
        .arg(pid.to_string())
        .arg(socket_path)
        .arg(target_path)
        .arg(is_bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

fn update_endpoints(settings: &Settings) -> Option<Vec<Url>> {
    if !settings.github_proxy_enabled {
        return None;
    }
    let endpoints: Vec<Url> = github_proxy_candidates(UPDATE_ENDPOINT, settings, false)
        .into_iter()
        .filter_map(|candidate| Url::parse(&candidate).ok())
        .collect();
    if endpoints.is_empty() {
        None
    } else {
        Some(endpoints)
    }
}

fn build_updater(app: &AppHandle, settings: &Settings) -> Result<Updater> {
    let mut builder = app.updater_builder().timeout(Duration::from_secs(60));
    if let Some(endpoints) = update_endpoints(settings) {
        builder = builder
            .endpoints(endpoints)
            .context("配置 GitHub 更新加速线路失败")?;
    }
    if let Some(proxy) = updater_proxy(settings) {
        builder = builder.proxy(proxy);
    }
    builder.build().context("初始化更新器失败")
}

fn publish_update_status(
    manager: &Arc<TaskManager>,
    slot: &Arc<Mutex<UpdateStatus>>,
    status: UpdateStatus,
) {
    if let Ok(mut current) = slot.lock() {
        *current = status.clone();
    }
    manager.broadcast(
        "update:status",
        serde_json::to_value(&status).unwrap_or(Value::Null),
    );
}

fn resolve_amule_dir(app: &AppHandle) -> Option<PathBuf> {
    let platform = if cfg!(windows) {
        if cfg!(target_arch = "x86_64") {
            "win-x64"
        } else {
            "win-arm64"
        }
    } else {
        "macos-universal"
    };
    let exe = if cfg!(windows) {
        "amuled.exe"
    } else {
        "amuled"
    };
    let mut candidates = Vec::new();
    if let Ok(resource) = app.path().resource_dir() {
        candidates.push(resource.join("resources").join("amule").join(platform));
        candidates.push(resource.join("amule").join(platform));
        candidates.push(resource.join("resources").join("amule"));
        candidates.push(resource.join("amule"));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(
            PathBuf::from(manifest)
                .join("resources")
                .join("amule")
                .join(platform),
        );
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("bin").join(exe).is_file() || dir.join(exe).is_file())
}

/// 从剪贴板文本中提取 ED2K / BT（magnet 或 .torrent URL）链接。
fn extract_clipboard_link(text: &str) -> Option<(String, &'static str)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let candidate = if let Some(start) = lower.find("ed2k://") {
        let rest = &text[start..];
        let end = rest.find("|/").map(|i| i + 2).unwrap_or(rest.len());
        rest[..end].trim().to_string()
    } else if let Some(start) = lower.find("magnet:?") {
        let rest = &text[start..];
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        rest[..end].trim().to_string()
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        let end = text.find(|c: char| c.is_whitespace()).unwrap_or(text.len());
        text[..end].trim().to_string()
    } else {
        return None;
    };
    let kind = detect_kind(&candidate.to_ascii_lowercase());
    if kind == "http" || candidate.len() <= 8 {
        return None;
    }
    Some((candidate, kind))
}

/// 轮询系统剪贴板：一旦出现新的 ED2K / BT 链接，就弹出新建任务窗口。
/// 开关由设置项 monitorClipboard 实时控制，无需重启。
fn spawn_clipboard_monitor(
    hooks: Arc<TauriHooks>,
    settings: Arc<SettingsStore>,
    platform: Arc<dyn PlatformService>,
    quitting: Arc<AtomicBool>,
) {
    tauri::async_runtime::spawn(async move {
        // 启动时先记录当前剪贴板，避免把启动前已有的链接误当成新复制的内容。
        let mut last = tokio::task::spawn_blocking({
            let platform = platform.clone();
            move || platform.read_clipboard()
        })
        .await
        .unwrap_or_default();

        loop {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            if quitting.load(Ordering::SeqCst) {
                break;
            }
            let current = tokio::task::spawn_blocking({
                let platform = platform.clone();
                move || platform.read_clipboard()
            })
            .await
            .unwrap_or_default();
            if current.is_empty() || current == last {
                continue;
            }
            last = current.clone();
            if !settings.get().monitor_clipboard {
                continue;
            }
            if let Some((link, kind)) = extract_clipboard_link(&current) {
                let payload = json!({ "url": link, "kind": kind });
                if let Err(err) = hooks.open_new_download(payload).await {
                    tracing::warn!(error = %err, "剪贴板监视打开新建任务窗口失败");
                }
            }
        }
    });
}

/// 把命令行里传入的链接或 .torrent 路径转换成新建下载窗口的预填参数。
fn prefill_from_arg(raw: &str, cwd: Option<&str>) -> Option<Value> {
    let arg = raw.trim().trim_matches('"');
    if arg.is_empty() {
        return None;
    }
    let lower = arg.to_ascii_lowercase();
    if lower.starts_with("ed2k://") {
        // macOS 可能把 ed2k 链接中的竖线转义为 %7C，还原后再交给解析器。
        let decoded =
            percent_encoding::percent_decode_str(&arg["ed2k://".len()..]).decode_utf8_lossy();
        return Some(json!({ "url": format!("ed2k://{decoded}") }));
    }
    if lower.starts_with("magnet:") || lower.starts_with("http://") || lower.starts_with("https://")
    {
        return Some(json!({ "url": arg }));
    }
    if lower.starts_with("file://") {
        if let Ok(url) = Url::parse(arg) {
            if let Ok(path) = url.to_file_path() {
                let is_torrent = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("torrent"))
                    .unwrap_or(false);
                if is_torrent {
                    return Some(json!({ "kind": "bt", "torrentPath": path.to_string_lossy() }));
                }
            }
        }
    }
    if lower.ends_with(".torrent") {
        let path = PathBuf::from(arg);
        let path = if path.is_absolute() {
            path
        } else {
            let base = cwd
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            base.join(path)
        };
        return Some(json!({ "kind": "bt", "torrentPath": path.to_string_lossy() }));
    }
    None
}

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            let Some(runtime) = app.try_state::<AppRuntime>() else {
                return;
            };
            let hooks = runtime.hooks.clone();
            let payload = argv
                .iter()
                .skip(1)
                .find_map(|arg| prefill_from_arg(arg, Some(cwd.as_str())));
            tauri::async_runtime::spawn(async move {
                match payload {
                    Some(payload) => {
                        let _ = hooks.open_new_download(payload).await;
                    }
                    None => {
                        let _ = hooks.open_main().await;
                    }
                }
            });
        }))
        .setup(|app| {
            let startup_payload = std::env::args()
                .skip(1)
                .find_map(|arg| prefill_from_arg(&arg, None));
            let data_dir = paths::data_dir();
            paths::ensure_dir(&data_dir)?;
            let _ = paths::migrate_legacy_data(&data_dir);
            let settings = Arc::new(SettingsStore::load(&data_dir)?);
            let amule_dir = resolve_amule_dir(app.handle());
            // 先创建管理器并启动 HTTP/UI；BT/ED2K 引擎在后台异步初始化。
            let manager = TaskManager::new(Some(data_dir.clone()), settings.clone())?;
            tauri::async_runtime::block_on(async {
                manager.start_scheduler();
            });
            let engine_manager = manager.clone();
            tauri::async_runtime::spawn(async move {
                engine_manager.init_engines(EnginePaths { amule_dir }).await;
            });
            let extension_dir = ensure_extension_assets(&data_dir)?;
            let platform: Arc<dyn PlatformService> =
                Arc::new(SystemPlatform::new(extension_dir, !cfg!(debug_assertions)));
            let quitting = Arc::new(AtomicBool::new(false));
            let hooks = Arc::new(TauriHooks::new(
                app.handle().clone(),
                manager.clone(),
                settings.clone(),
                quitting.clone(),
            ));
            app.manage(AppRuntime {
                hooks: hooks.clone(),
                manager: manager.clone(),
                quitting: quitting.clone(),
            });

            let listener = tauri::async_runtime::block_on(fdm_server::bind_default())?;
            let server_state = ServerState {
                manager: manager.clone(),
                settings: settings.clone(),
                platform: platform.clone(),
                hooks: hooks.clone(),
                data_dir: data_dir.clone(),
                duplicate_requests: Arc::new(tokio::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                duplicate_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                last_browser_seen: Arc::new(Mutex::new(None)),
            };
            tauri::async_runtime::spawn(async move {
                if let Err(err) = serve(listener, server_state).await {
                    tracing::error!(error = %err, "FDM HTTP 服务退出");
                }
            });

            let background = std::env::args().any(|a| a == "--background");
            let show_extension_guide =
                !background && startup_payload.is_none() && !settings.get().extension_guide_shown;
            let main_url = Url::parse(BASE_URL)?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(main_url))
                .title("FDM · 极速多线程下载器")
                .inner_size(780.0, 500.0)
                .min_inner_size(780.0, 500.0)
                .resizable(true)
                .center()
                .visible(!background)
                .build()?;

            if show_extension_guide {
                let hooks = hooks.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let _ = hooks.open_extension_guide().await;
                });
            }

            spawn_clipboard_monitor(
                hooks.clone(),
                settings.clone(),
                platform.clone(),
                quitting.clone(),
            );

            #[cfg(not(debug_assertions))]
            spawn_startup_update_check(hooks.clone(), platform.clone());

            if let Some(payload) = startup_payload {
                let hooks = hooks.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = hooks.open_new_download(payload).await;
                });
            }

            if !cfg!(debug_assertions) {
                let _ = platform.sync_auto_launch(settings.get().auto_launch);
                let _ = platform.sync_file_associations(settings.get().file_associations);
            }

            let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let new_item = MenuItem::with_id(app, "new", "新建下载", true, None::<&str>)?;
            let settings_item = MenuItem::with_id(app, "settings", "选项", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[
                    &show_item,
                    &new_item,
                    &settings_item,
                    &separator,
                    &quit_item,
                ],
            )?;
            let mut tray = TrayIconBuilder::new()
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    let Some(runtime) = app.try_state::<AppRuntime>() else {
                        return;
                    };
                    let hooks = runtime.hooks.clone();
                    match event.id.as_ref() {
                        "show" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = hooks.open_main().await;
                            });
                        }
                        "new" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = hooks.open_new_download(json!({})).await;
                            });
                        }
                        "settings" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = hooks.open_settings().await;
                            });
                        }
                        "quit" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = hooks.quit().await;
                            });
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(runtime) = app.try_state::<AppRuntime>() {
                            let hooks = runtime.hooks.clone();
                            tauri::async_runtime::spawn(async move {
                                let _ = hooks.open_main().await;
                            });
                        }
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let quitting = window
                        .app_handle()
                        .try_state::<AppRuntime>()
                        .map(|s| s.quitting.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    if !quitting {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building FDM desktop");

    app.run(|app_handle, event| {
        #[cfg(target_os = "macos")]
        handle_macos_run_event(app_handle, event);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app_handle, event);
        }
    });
}

/// macOS 通过 LaunchServices 把打开的链接或 .torrent 文件交给应用。
#[cfg(target_os = "macos")]
fn handle_macos_run_event(app_handle: &tauri::AppHandle, event: tauri::RunEvent) {
    match event {
        tauri::RunEvent::Opened { urls } => {
            let Some(runtime) = app_handle.try_state::<AppRuntime>() else {
                return;
            };
            let hooks = runtime.hooks.clone();
            let payload = urls
                .iter()
                .find_map(|url| prefill_from_arg(url.as_str(), None));
            if let Some(payload) = payload {
                tauri::async_runtime::spawn(async move {
                    let _ = hooks.open_new_download(payload).await;
                });
            }
        }
        tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            if !has_visible_windows {
                if let Some(runtime) = app_handle.try_state::<AppRuntime>() {
                    let hooks = runtime.hooks.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = hooks.open_main().await;
                    });
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    #[test]
    fn parses_links_and_torrent_paths() {
        let magnet = prefill_from_arg("magnet:?xt=urn:btih:abc", None).unwrap();
        assert_eq!(magnet["url"], "magnet:?xt=urn:btih:abc");

        let ed2k = prefill_from_arg(
            "ed2k://|file|a.iso|1|8b54a3c20fae9e4b9f7e0c2c8c01b6b1|/",
            None,
        )
        .unwrap();
        assert_eq!(
            ed2k["url"],
            "ed2k://|file|a.iso|1|8b54a3c20fae9e4b9f7e0c2c8c01b6b1|/"
        );

        let absolute = prefill_from_arg(r"D:\Downloads\a.torrent", None).unwrap();
        assert_eq!(absolute["kind"], "bt");
        assert_eq!(absolute["torrentPath"], r"D:\Downloads\a.torrent");

        let relative = prefill_from_arg("a.torrent", Some(r"D:\Downloads")).unwrap();
        assert_eq!(relative["torrentPath"], r"D:\Downloads\a.torrent");

        let file_url = prefill_from_arg("file:///C:/Users/test/a.torrent", None).unwrap();
        assert_eq!(file_url["kind"], "bt");
        assert!(file_url["torrentPath"]
            .as_str()
            .unwrap()
            .ends_with("a.torrent"));
        assert!(prefill_from_arg("file:///C:/Users/test/a.zip", None).is_none());

        let encoded_ed2k = prefill_from_arg(
            "ed2k://%7Cfile%7Ca.iso%7C1%7C8b54a3c20fae9e4b9f7e0c2c8c01b6b1%7C/",
            None,
        )
        .unwrap();
        assert_eq!(
            encoded_ed2k["url"],
            "ed2k://|file|a.iso|1|8b54a3c20fae9e4b9f7e0c2c8c01b6b1|/"
        );

        assert!(prefill_from_arg("--background", None).is_none());
    }

    #[test]
    fn detects_macos_bundle_path() {
        let app = std::path::Path::new("/Applications/FDM.app/Contents/MacOS/fdm");
        assert_eq!(
            find_bundle_path(app),
            Some(PathBuf::from("/Applications/FDM.app"))
        );

        let non_bundle = std::path::Path::new("/usr/local/bin/fdm");
        assert_eq!(find_bundle_path(non_bundle), None);
    }
}
