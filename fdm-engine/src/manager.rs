use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use fdm_core::paths::{atomic_write, resolve_data_dir};
use fdm_core::*;
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::Serialize;
use serde_json::Value;
use tokio::fs::{File as AsyncFile, OpenOptions as AsyncOpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::bt::{BtEngine, BtSource, BtState};
use crate::ed2k::{Ed2kEngine, Ed2kState};
use crate::probe::{
    fetch_torrent_bytes_candidates, headers_map, probe_candidates, HttpClientCache,
};
use crate::task::TaskEntry;

fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

async fn wait_task_stopped(entry: &Arc<TaskEntry>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while entry.is_running() && Instant::now() < deadline {
        sleep(Duration::from_millis(20)).await;
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize_path(a.to_string_lossy().as_ref()) == normalize_path(b.to_string_lossy().as_ref())
}

/// 删除文件；Windows 上刚取消的任务可能短暂占用句柄，因此做少量重试。
async fn remove_file_with_retry(path: &Path, label: &str) {
    if path.as_os_str().is_empty() {
        return;
    }
    for attempt in 0..REMOVE_RETRIES {
        match fs::remove_file(path) {
            Ok(()) => return,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) if attempt + 1 == REMOVE_RETRIES => {
                tracing::warn!(error = %err, path = %path.display(), label = label, "删除文件失败");
            }
            Err(_) => sleep(REMOVE_RETRY_DELAY).await,
        }
    }
}

const PROGRESS_BATCH_BYTES: usize = 256 * 1024;
const PROGRESS_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const CFG_SAVE_INTERVAL: Duration = Duration::from_secs(1);
const MIN_SPLIT_BYTES: u64 = 1024 * 1024;
const MAX_SPLITS_PER_CONNECTION: usize = 4;
const REMOVE_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const REMOVE_RETRIES: usize = 3;
const REMOVE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub enum AddTaskResult {
    /// 新任务已创建
    Added(TaskSnapshot),
    /// 命中已有任务：action=0 跳过，action=3 复用/恢复
    Existing { task: TaskSnapshot, action: u8 },
    /// 需要用户选择重复下载策略
    NeedsChoice {
        existing: TaskSnapshot,
        default_action: u8,
    },
    /// 目标文件名冲突（磁盘或下载列表）
    NameConflict {
        filename: String,
        path: String,
        reason: String,
        suggested: String,
        existing_id: Option<i64>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct EnginePaths {
    pub amule_dir: Option<PathBuf>,
}

struct BufferedWriter {
    file: AsyncFile,
    start_offset: u64,
    pending: Vec<u8>,
    last_flush: Instant,
}

impl BufferedWriter {
    async fn new(mut file: AsyncFile, offset: u64) -> Result<Self> {
        file.seek(SeekFrom::Start(offset)).await?;
        Ok(Self {
            file,
            start_offset: offset,
            pending: Vec::with_capacity(PROGRESS_BATCH_BYTES),
            last_flush: Instant::now(),
        })
    }

    fn offset(&self) -> u64 {
        self.start_offset + self.pending.len() as u64
    }

    fn should_flush(&self) -> bool {
        self.pending.len() >= PROGRESS_BATCH_BYTES
            || self.last_flush.elapsed() >= PROGRESS_FLUSH_INTERVAL
    }

    /// 只写入 limit_exclusive 之前的字节；分片被切小时丢弃越界缓冲。
    async fn flush_limit(&mut self, limit_exclusive: u64) -> Result<u64> {
        if self.pending.is_empty() {
            self.last_flush = Instant::now();
            return Ok(self.start_offset);
        }
        let allowed = if limit_exclusive == u64::MAX {
            self.pending.len() as u64
        } else {
            limit_exclusive.saturating_sub(self.start_offset)
        };
        if allowed == 0 {
            self.pending.clear();
            self.last_flush = Instant::now();
            return Ok(self.start_offset);
        }
        if self.pending.len() as u64 > allowed {
            self.pending.truncate(allowed as usize);
        }
        self.file.write_all(&self.pending).await?;
        self.file.flush().await?;
        self.start_offset += self.pending.len() as u64;
        self.pending.clear();
        self.last_flush = Instant::now();
        Ok(self.start_offset)
    }
}

struct PartClaim {
    manager: Arc<TaskManager>,
    key: String,
}

impl Drop for PartClaim {
    fn drop(&mut self) {
        // 即便锁曾因 panic 中毒，也要恢复内部集合，避免占用标记永久泄漏。
        let mut claims = match self.manager.part_claims.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        claims.remove(&self.key);
    }
}

pub struct TaskManager {
    pub data_dir: PathBuf,
    pub tasks_file: PathBuf,
    pub settings: Arc<SettingsStore>,
    tasks: RwLock<BTreeMap<i64, Arc<TaskEntry>>>,
    events: broadcast::Sender<Event>,
    next_id: AtomicI64,
    /// 串行化“查重 + 插入”，避免并发点击产生两个相同任务。
    add_lock: AsyncMutex<()>,
    http_client: HttpClientCache,
    /// 正在被写入的 `.fdmpart` 路径集合，避免两个任务同时写同一分片文件。
    part_claims: StdMutex<HashSet<String>>,
    bt: StdMutex<Option<Arc<BtEngine>>>,
    ed2k: StdMutex<Option<Arc<Ed2kEngine>>>,
    /// aMule 资源目录；初始化失败后“重新连接”需要用它重建引擎。
    amule_dir: StdMutex<Option<PathBuf>>,
    /// 串行化 ED2K 引擎重建，避免并发点击导致端口冲突。
    ed2k_restart_lock: AsyncMutex<()>,
}

impl TaskManager {
    pub fn new(data_dir: Option<PathBuf>, settings: Arc<SettingsStore>) -> Result<Arc<Self>> {
        Self::build(data_dir, settings, None, None)
    }

    /// 返回当前已就绪的 BT 引擎副本；初始化完成前为 None。
    pub fn bt_engine(&self) -> Option<Arc<BtEngine>> {
        self.bt.lock().ok().and_then(|engine| engine.clone())
    }

    /// 返回当前已就绪的 ED2K 引擎副本；初始化完成前为 None。
    pub fn ed2k_engine(&self) -> Option<Arc<Ed2kEngine>> {
        self.ed2k.lock().ok().and_then(|engine| engine.clone())
    }

    fn set_engines(&self, bt: Option<Arc<BtEngine>>, ed2k: Option<Arc<Ed2kEngine>>) {
        if let Ok(mut current) = self.bt.lock() {
            *current = bt;
        }
        self.set_ed2k(ed2k);
    }

    fn set_ed2k(&self, ed2k: Option<Arc<Ed2kEngine>>) {
        if let Ok(mut current) = self.ed2k.lock() {
            *current = ed2k;
        }
    }

    /// 异步初始化 BT/ED2K 引擎；不阻塞 UI 和 HTTP 服务启动。
    pub async fn init_engines(self: &Arc<Self>, paths: EnginePaths) {
        let resolved = self.data_dir.clone();
        let settings = self.settings.clone();
        let bt = match BtEngine::new(&resolved, settings.clone()).await {
            Ok(engine) => Some(engine),
            Err(err) => {
                tracing::warn!(error = %err, "BT 引擎未启动");
                None
            }
        };
        let ed2k = self.create_ed2k_engine(paths.amule_dir.clone()).await;
        // 即使首次启动失败也保存资源目录，之后可通过“重新连接”重建。
        if let Ok(mut dir) = self.amule_dir.lock() {
            *dir = paths.amule_dir;
        }
        self.set_engines(bt, ed2k);
        self.reconcile_p2p().await;
    }

    async fn create_ed2k_engine(&self, amule_dir: Option<PathBuf>) -> Option<Arc<Ed2kEngine>> {
        let Some(dir) = amule_dir else {
            return None;
        };
        match Ed2kEngine::new(&self.data_dir, self.settings.clone(), dir).await {
            Ok(engine) => Some(engine),
            Err(err) => {
                tracing::warn!(error = %err, "ED2K 引擎未启动");
                None
            }
        }
    }

    /// 重建 ED2K 引擎。启动失败或引擎失活后，“重新连接”会走这里。
    pub async fn restart_ed2k_engine(self: &Arc<Self>) -> Result<Option<Arc<Ed2kEngine>>> {
        let _guard = self.ed2k_restart_lock.lock().await;
        if let Some(old) = self.ed2k_engine() {
            old.shutdown().await;
        }
        self.set_ed2k(None);
        if !self.settings.get().ed2k.enabled {
            return Ok(None);
        }
        let dir = self.amule_dir.lock().ok().and_then(|dir| dir.clone());
        let Some(dir) = dir else {
            return Ok(None);
        };
        let engine = Ed2kEngine::new(&self.data_dir, self.settings.clone(), dir).await?;
        self.set_ed2k(Some(engine.clone()));
        self.reconcile_p2p().await;
        Ok(Some(engine))
    }

    fn build(
        data_dir: Option<PathBuf>,
        settings: Arc<SettingsStore>,
        bt: Option<Arc<BtEngine>>,
        ed2k: Option<Arc<Ed2kEngine>>,
    ) -> Result<Arc<Self>> {
        let data_dir = resolve_data_dir(data_dir);
        fdm_core::paths::ensure_dir(&data_dir)?;
        let _ = fdm_core::paths::migrate_legacy_data(&data_dir);
        let tasks_file = data_dir.join("tasks.json");
        let mut tasks = BTreeMap::new();
        let mut max_id = 0_i64;
        if tasks_file.exists() {
            match fdm_core::paths::read_json::<Vec<SavedTask>>(&tasks_file) {
                Ok(saved) => {
                    let settings_snapshot = settings.get();
                    let global_rate = settings_snapshot.max_rate_kb.saturating_mul(1024);
                    for item in saved {
                        if item.id <= 0 {
                            continue;
                        }
                        max_id = max_id.max(item.id);
                        let data = TaskData::from_saved(
                            item,
                            Path::new(&settings_snapshot.default_dir),
                            settings_snapshot.default_connections as usize,
                            global_rate,
                        );
                        tasks.insert(data.id, Arc::new(TaskEntry::new(data)));
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %tasks_file.display(), "读取历史任务失败")
                }
            }
        }
        let (events, _) = broadcast::channel(2048);
        Ok(Arc::new(Self {
            data_dir,
            tasks_file,
            settings,
            tasks: RwLock::new(tasks),
            events,
            next_id: AtomicI64::new(max_id + 1),
            add_lock: AsyncMutex::new(()),
            http_client: HttpClientCache::default(),
            part_claims: StdMutex::new(HashSet::new()),
            bt: StdMutex::new(bt),
            ed2k: StdMutex::new(ed2k),
            amule_dir: StdMutex::new(None),
            ed2k_restart_lock: AsyncMutex::new(()),
        }))
    }

    /// 全局复用的 HTTP 连接池；代理配置变化时下一次调用自动重建。
    pub fn http_client(&self) -> Result<Arc<reqwest::Client>> {
        self.http_client.get(&self.settings.get())
    }

    /// 某个候选地址失败时，把共享索引推进到下一个 GitHub 代理；返回 true 表示应重新发起请求。
    fn advance_proxy_candidate(
        index: &AtomicUsize,
        candidates: &[String],
        used_index: usize,
        reason: &str,
    ) -> bool {
        let mut current = index.load(Ordering::SeqCst);
        if current != used_index {
            // 其他 worker 已经切换，当前请求直接用新候选重试。
            return true;
        }
        if current + 1 >= candidates.len() {
            return false;
        }
        while current + 1 < candidates.len() {
            match index.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    tracing::warn!(
                        from = %candidates[current],
                        to = %candidates[current + 1],
                        reason,
                        "GitHub 加速：切换候选地址"
                    );
                    return true;
                }
                Err(actual) => {
                    if actual != used_index {
                        return true;
                    }
                    current = actual;
                }
            }
        }
        false
    }

    /// 响应体长度与预期总大小不一致，通常是镜像返回的 HTML 错误页。
    fn response_length_mismatch(resp: &reqwest::Response, expected: Option<u64>) -> bool {
        let Some(expected) = expected else {
            return false;
        };
        resp.headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|actual| actual != expected)
            .unwrap_or(false)
    }

    /// 206 响应的 Content-Range 起点必须与当前分片偏移一致。
    fn content_range_mismatch(resp: &reqwest::Response, expected_start: u64) -> bool {
        let Some(value) = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
        else {
            // 206 响应必须带 Content-Range；缺失时不能把 body 直接写到分片偏移。
            return true;
        };
        let lower = value.trim().to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix("bytes ") else {
            return true;
        };
        let Some((start, _)) = rest.split_once('-') else {
            return true;
        };
        start
            .trim()
            .parse::<u64>()
            .map(|start| start != expected_start)
            .unwrap_or(true)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn broadcast(&self, event: &str, data: Value) {
        let _ = self.events.send(Event {
            event: event.to_string(),
            data,
        });
    }

    async fn emit_entry(&self, entry: &Arc<TaskEntry>) {
        let snap = entry.snapshot().await;
        if let Ok(value) = serde_json::to_value(&snap) {
            self.broadcast("task:update", value);
        }
    }

    pub async fn save_tasks(&self) -> Result<()> {
        let entries = self.entries();
        let mut saved = Vec::with_capacity(entries.len());
        for entry in entries {
            let state = entry.state.lock().await;
            saved.push(state.saved());
        }
        saved.sort_by_key(|t| t.id);
        let text = serde_json::to_vec_pretty(&saved)?;
        atomic_write(&self.tasks_file, &text)?;
        Ok(())
    }

    fn entries(&self) -> Vec<Arc<TaskEntry>> {
        self.tasks
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn list_tasks(&self) -> Vec<TaskSnapshot> {
        let mut out = Vec::new();
        for entry in self.entries() {
            out.push(entry.snapshot().await);
        }
        out.sort_by_key(|t| t.id);
        out
    }

    pub async fn get_task(&self, id: i64) -> Option<TaskSnapshot> {
        let entry = self.tasks.read().ok()?.get(&id).cloned()?;
        Some(entry.snapshot().await)
    }

    async fn active_count(&self) -> usize {
        let mut count = 0;
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if is_active_status(&state.status) {
                count += 1;
            }
        }
        count
    }

    /// 兼容旧调用方：不弹窗，重复时按“完成则显示/否则继续”（动作 3）处理。
    pub async fn add_task(self: &Arc<Self>, opts: &Value) -> Result<TaskSnapshot> {
        let mut effective = opts.clone();
        if effective.get("duplicateAction").is_none() && effective.get("duplicate_action").is_none()
        {
            if let Some(obj) = effective.as_object_mut() {
                obj.insert("duplicateAction".to_string(), serde_json::json!(3));
            }
        }
        match self.add_task_checked(&effective).await? {
            AddTaskResult::Added(snap) => Ok(snap),
            AddTaskResult::Existing { task, .. } => Ok(task),
            AddTaskResult::NeedsChoice { existing, .. } => Ok(existing),
            AddTaskResult::NameConflict { path, .. } => Err(anyhow!("目标文件已存在: {path}")),
        }
    }

    /// 带重复下载策略的添加任务入口。
    pub async fn add_task_checked(self: &Arc<Self>, opts: &Value) -> Result<AddTaskResult> {
        let _guard = self.add_lock.lock().await;

        let url = opts
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            bail!("URL 不能为空");
        }
        let requested_kind = opts
            .get("kind")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "http" | "bt" | "ed2k"));
        let kind = requested_kind.unwrap_or_else(|| {
            let looks_like_torrent_file = opts
                .get("filename")
                .and_then(Value::as_str)
                .map(|name| name.to_ascii_lowercase().ends_with(".torrent"))
                .unwrap_or(false);
            if looks_like_torrent_file
                && (url.starts_with("http://") || url.starts_with("https://"))
            {
                "bt".to_string()
            } else {
                fdm_core::detect_kind(&url).to_string()
            }
        });
        if kind != "http" {
            return self.add_p2p_task(opts, &url, &kind).await;
        }
        let settings = self.settings.get();
        let mut dir = opts
            .get("dir")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&settings.default_dir)
            .to_string();
        if !Path::new(&dir).is_absolute() {
            bail!("下载目录必须是绝对路径");
        }
        let requested_filename = opts
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let match_filename = fdm_core::resolve_filename(&url, requested_filename.as_deref());
        let connections = opts
            .get("connections")
            .and_then(Value::as_u64)
            .unwrap_or(settings.default_connections as u64) as usize;
        let explicit_rate = opts
            .get("rate")
            .or_else(|| opts.get("max_rate"))
            .or_else(|| opts.get("maxRate"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rate_explicit = explicit_rate > 0;
        let max_rate = if rate_explicit {
            explicit_rate
        } else {
            settings.max_rate_kb.saturating_mul(1024)
        };
        let mut custom_headers = HashMap::new();
        if let Some(obj) = opts.get("headers").and_then(Value::as_object) {
            for (k, v) in obj {
                let text = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                custom_headers.insert(k.clone(), text);
            }
        }

        let explicit_action = opts
            .get("duplicateAction")
            .or_else(|| opts.get("duplicate_action"))
            .and_then(Value::as_u64)
            .map(|v| v.min(3) as u8);
        let remember = opts
            .get("rememberDuplicate")
            .or_else(|| opts.get("remember_duplicate"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut final_filename_opt = requested_filename.clone();
        let mut final_filename = fdm_core::resolve_filename(&url, final_filename_opt.as_deref());
        let mut duplicate_action: Option<u8> = None;

        if let Some(existing) = self.find_duplicate(&url, &match_filename).await {
            let action = explicit_action.or_else(|| {
                if settings.remember_duplicate_action && settings.duplicate_action != 0 {
                    Some(settings.duplicate_action.min(3))
                } else {
                    None
                }
            });
            let Some(action) = action else {
                return Ok(AddTaskResult::NeedsChoice {
                    existing,
                    default_action: settings.duplicate_action.min(3),
                });
            };
            duplicate_action = Some(action);
            if remember && action != 0 {
                let _ = self.settings.update_patch(&serde_json::json!({
                    "duplicateAction": action,
                    "rememberDuplicateAction": true
                }));
            }
            match action {
                0 => {
                    return Ok(AddTaskResult::Existing {
                        task: existing,
                        action: 0,
                    });
                }
                3 => {
                    self.activate_existing(&existing).await;
                    let task = self.get_task(existing.id).await.unwrap_or(existing);
                    return Ok(AddTaskResult::Existing { task, action: 3 });
                }
                2 => {
                    // 覆盖：复用已有任务的目标路径
                    dir = existing.dir.clone();
                    final_filename = existing.filename.clone();
                    final_filename_opt = Some(final_filename.clone());
                }
                1 => {
                    final_filename = self.unique_filename(&dir, &match_filename).await;
                    final_filename_opt = Some(final_filename.clone());
                }
                _ => {}
            }
        }

        // 文件名冲突（磁盘 / 下载列表）；覆盖模式跳过。
        let explicit_file_action = opts
            .get("fileAction")
            .or_else(|| opts.get("file_action"))
            .and_then(Value::as_u64)
            .map(|v| v.min(2) as u8);
        let remember_file = opts
            .get("rememberFileConflict")
            .or_else(|| opts.get("remember_file_conflict"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let file_action = explicit_file_action.or_else(|| {
            if settings.remember_file_conflict_action && settings.file_conflict_action != 0 {
                Some(settings.file_conflict_action.min(2))
            } else {
                None
            }
        });
        if let Some(action) = file_action {
            if remember_file && action != 0 {
                let _ = self.settings.update_patch(&serde_json::json!({
                    "fileConflictAction": action,
                    "rememberFileConflictAction": true
                }));
            }
        }
        // 仅真正存在同名冲突时才套用“同名文件冲突”策略，避免无冲突时也被改名。
        if duplicate_action != Some(2) {
            let list_conflict = self.find_name_conflict(&dir, &final_filename).await;
            let disk_path = Path::new(&dir).join(&final_filename);
            let disk_conflict = disk_path.exists();
            if list_conflict.is_some() || disk_conflict {
                match file_action {
                    Some(1) => {
                        final_filename = self.unique_filename(&dir, &match_filename).await;
                        final_filename_opt = Some(final_filename.clone());
                    }
                    Some(2) => {
                        final_filename_opt = Some(final_filename.clone());
                    }
                    _ => {
                        if let Some((existing_id, _)) = list_conflict {
                            return Ok(AddTaskResult::NameConflict {
                                filename: final_filename.clone(),
                                path: Path::new(&dir)
                                    .join(&final_filename)
                                    .to_string_lossy()
                                    .into_owned(),
                                reason: "list".to_string(),
                                suggested: self.unique_filename(&dir, &final_filename).await,
                                existing_id: Some(existing_id),
                            });
                        }
                        return Ok(AddTaskResult::NameConflict {
                            filename: final_filename.clone(),
                            path: disk_path.to_string_lossy().into_owned(),
                            reason: "disk".to_string(),
                            suggested: self.unique_filename(&dir, &final_filename).await,
                            existing_id: None,
                        });
                    }
                }
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let data = TaskData::new(
            id,
            url,
            dir,
            final_filename_opt,
            connections,
            max_rate,
            rate_explicit,
            custom_headers,
        );
        let entry = Arc::new(TaskEntry::new(data));
        self.tasks
            .write()
            .map_err(|_| anyhow!("任务表锁损坏"))?
            .insert(id, entry.clone());
        self.save_tasks().await?;
        self.emit_entry(&entry).await;

        let start = opts.get("start").and_then(Value::as_bool).unwrap_or(true);
        let start_paused = opts
            .get("startPaused")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !start || start_paused {
            let mut state = entry.state.lock().await;
            state.status = STATUS_PAUSED.to_string();
            drop(state);
            self.emit_entry(&entry).await;
        } else {
            let max_concurrent = self.settings.get().max_concurrent.max(1) as usize;
            if self.active_count().await < max_concurrent {
                self.start_task(entry.clone()).await;
            } else {
                let mut state = entry.state.lock().await;
                state.status = STATUS_QUEUED.to_string();
                drop(state);
                self.emit_entry(&entry).await;
            }
        }
        Ok(AddTaskResult::Added(entry.snapshot().await))
    }

    /// BT / ED2K 任务的添加入口（不走 HTTP 的 Range 探测与分块逻辑）。
    async fn add_p2p_task(
        self: &Arc<Self>,
        opts: &Value,
        url: &str,
        kind: &str,
    ) -> Result<AddTaskResult> {
        let settings = self.settings.get();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut dir = opts
            .get("dir")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| settings.default_dir.clone());
        if !Path::new(&dir).is_absolute() {
            bail!("下载目录必须是绝对路径");
        }
        let mut filename = opts
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_default();
        let explicit_rate = opts
            .get("rate")
            .or_else(|| opts.get("max_rate"))
            .or_else(|| opts.get("maxRate"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rate_explicit = explicit_rate > 0;
        let max_rate = if rate_explicit {
            explicit_rate
        } else {
            settings.max_rate_kb.saturating_mul(1024)
        };

        let mut info_hash: Option<String> = None;
        let mut ed2k_hash: Option<String> = None;
        let total: Option<u64>;
        let mut files: Vec<TorrentFileInfo> = Vec::new();
        let mut torrent_path: Option<String> = None;
        let mut ed2k_link: Option<String> = None;

        if kind == "bt" {
            let engine = self.bt_engine().ok_or_else(|| anyhow!("BT 引擎未启用"))?;
            let raw_path = opts
                .get("torrentPath")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);
            let raw_url = opts
                .get("torrentUrl")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    if url.starts_with("http://") || url.starts_with("https://") {
                        Some(url.to_string())
                    } else {
                        None
                    }
                });
            let bytes = if let Some(path) = raw_path {
                Some(std::fs::read(&path).with_context(|| format!("读取种子文件失败：{path}"))?)
            } else if let Some(torrent_url) = raw_url {
                let client = self.http_client().context("创建种子下载客户端失败")?;
                let candidates = github_proxy_candidates(&torrent_url, &settings, false);
                Some(
                    fetch_torrent_bytes_candidates(client.as_ref(), &candidates)
                        .await
                        .context("下载种子文件失败")?,
                )
            } else {
                None
            };
            if let Some(data) = bytes {
                let info = engine.inspect_torrent(data.clone()).await?;
                info_hash = Some(info.info_hash.clone());
                if filename.trim().is_empty() || filename.to_ascii_lowercase().ends_with(".torrent")
                {
                    filename = info.name.clone();
                }
                total = Some(info.total);
                files = info.files.clone();
                let cache_dir = self.data_dir.join("bt").join("torrents");
                fdm_core::paths::ensure_dir(&cache_dir)?;
                let cache_path = cache_dir.join(format!("{id}.torrent"));
                fdm_core::paths::atomic_write(&cache_path, &data)?;
                torrent_path = Some(cache_path.to_string_lossy().into_owned());
            } else {
                let magnet = opts.get("magnet").and_then(Value::as_str).unwrap_or(url);
                let parsed =
                    fdm_core::parse_magnet(magnet).ok_or_else(|| anyhow!("无效的磁力链接"))?;
                info_hash = parsed.btih.clone();
                if filename.trim().is_empty() {
                    filename = parsed.name.clone().unwrap_or_else(|| {
                        parsed
                            .btih
                            .as_deref()
                            .map(|h| h[..8.min(h.len())].to_string())
                            .unwrap_or_else(|| "torrent".to_string())
                    });
                }
                total = parsed.size;
            }
        } else {
            let link = opts.get("ed2kLink").and_then(Value::as_str).unwrap_or(url);
            let (name, size, hash) = if let Some(parsed) = fdm_core::parse_ed2k_link(link) {
                (parsed.name, Some(parsed.size), parsed.hash)
            } else if let Some(magnet) = fdm_core::parse_magnet(link) {
                let hash = magnet
                    .ed2k_hash
                    .ok_or_else(|| anyhow!("无效的 ED2K 链接"))?;
                (
                    magnet.name.unwrap_or_else(|| "ed2k-file".to_string()),
                    magnet.size,
                    hash,
                )
            } else {
                return Err(anyhow!("无效的 ED2K 链接"));
            };
            ed2k_hash = Some(hash);
            if filename.trim().is_empty() {
                filename = name;
            }
            total = size;
            ed2k_link = Some(link.to_string());
        }

        let mut filename = if filename.trim().is_empty() {
            "download".to_string()
        } else {
            fdm_core::sanitize_filename(&filename)
        };
        let url_owned = if kind == "ed2k" {
            ed2k_link.clone().unwrap_or_else(|| url.to_string())
        } else if url.trim().is_empty() {
            torrent_path
                .clone()
                .unwrap_or_else(|| format!("torrent://{filename}"))
        } else {
            url.to_string()
        };

        let explicit_action = opts
            .get("duplicateAction")
            .or_else(|| opts.get("duplicate_action"))
            .and_then(Value::as_u64)
            .map(|v| v.min(3) as u8);
        let remember = opts
            .get("rememberDuplicate")
            .or_else(|| opts.get("remember_duplicate"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(existing) = self
            .find_p2p_duplicate(kind, info_hash.as_deref(), ed2k_hash.as_deref(), &url_owned)
            .await
        {
            let action = explicit_action.or_else(|| {
                if settings.remember_duplicate_action && settings.duplicate_action != 0 {
                    Some(settings.duplicate_action.min(3))
                } else {
                    None
                }
            });
            let Some(action) = action else {
                return Ok(AddTaskResult::NeedsChoice {
                    existing,
                    default_action: settings.duplicate_action.min(3),
                });
            };
            if remember && action != 0 {
                let _ = self.settings.update_patch(&serde_json::json!({
                    "duplicateAction": action,
                    "rememberDuplicateAction": true
                }));
            }
            match action {
                0 => {
                    return Ok(AddTaskResult::Existing {
                        task: existing,
                        action: 0,
                    })
                }
                3 => {
                    self.activate_existing(&existing).await;
                    let task = self.get_task(existing.id).await.unwrap_or(existing);
                    return Ok(AddTaskResult::Existing { task, action: 3 });
                }
                2 => {
                    dir = existing.dir.clone();
                    filename = existing.filename.clone();
                }
                1 => {
                    let unique = self.unique_filename(&dir, &filename).await;
                    if kind == "bt" {
                        dir = Path::new(&dir).join(unique).to_string_lossy().into_owned();
                    } else {
                        filename = unique;
                    }
                }
                _ => {}
            }
        }

        let target = Path::new(&dir).join(&filename);
        if target.exists() {
            let action = opts
                .get("fileAction")
                .or_else(|| opts.get("file_action"))
                .and_then(Value::as_u64)
                .map(|v| v.min(2) as u8)
                .or_else(|| {
                    if settings.remember_file_conflict_action && settings.file_conflict_action != 0
                    {
                        Some(settings.file_conflict_action.min(2))
                    } else {
                        None
                    }
                });
            match action {
                Some(1) => {
                    let unique = self.unique_filename(&dir, &filename).await;
                    if kind == "bt" {
                        dir = Path::new(&dir).join(unique).to_string_lossy().into_owned();
                    } else {
                        filename = unique;
                    }
                }
                Some(2) => {}
                _ => {
                    return Ok(AddTaskResult::NameConflict {
                        filename: filename.clone(),
                        path: target.to_string_lossy().into_owned(),
                        reason: "disk".to_string(),
                        suggested: self.unique_filename(&dir, &filename).await,
                        existing_id: None,
                    })
                }
            }
        }

        let mut data = TaskData::new(
            id,
            url_owned,
            &dir,
            Some(filename),
            settings.default_connections as usize,
            max_rate,
            rate_explicit,
            HashMap::new(),
        );
        data.kind = kind.to_string();
        data.info_hash = info_hash;
        data.ed2k_hash = ed2k_hash;
        data.torrent_path = torrent_path;
        data.files = files;
        data.total = total;
        let entry = Arc::new(TaskEntry::new(data));
        self.tasks
            .write()
            .map_err(|_| anyhow!("任务表锁损坏"))?
            .insert(id, entry.clone());
        self.save_tasks().await?;
        self.emit_entry(&entry).await;

        let start = opts.get("start").and_then(Value::as_bool).unwrap_or(true);
        let start_paused = opts
            .get("startPaused")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !start || start_paused {
            entry.state.lock().await.status = STATUS_PAUSED.to_string();
            self.emit_entry(&entry).await;
        } else {
            self.start_task(entry.clone()).await;
        }
        Ok(AddTaskResult::Added(entry.snapshot().await))
    }

    async fn find_p2p_duplicate(
        &self,
        kind: &str,
        info_hash: Option<&str>,
        ed2k_hash: Option<&str>,
        url: &str,
    ) -> Option<TaskSnapshot> {
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.kind != kind {
                continue;
            }
            let same_hash = match kind {
                "bt" => info_hash
                    .map(|want| {
                        state
                            .info_hash
                            .as_deref()
                            .map(|have| have.eq_ignore_ascii_case(want))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false),
                "ed2k" => ed2k_hash
                    .map(|want| {
                        state
                            .ed2k_hash
                            .as_deref()
                            .map(|have| have.eq_ignore_ascii_case(want))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false),
                _ => false,
            };
            if same_hash || (!url.is_empty() && state.url == url) {
                return Some(state.snapshot());
            }
        }
        None
    }

    async fn find_duplicate(&self, url: &str, filename: &str) -> Option<TaskSnapshot> {
        let canonical = fdm_core::canonical_url(url);
        let name = filename.to_lowercase();
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.filename.is_empty() {
                continue;
            }
            if fdm_core::canonical_url(&state.url) == canonical
                && state.filename.to_lowercase() == name
            {
                return Some(state.snapshot());
            }
        }
        None
    }

    async fn find_name_conflict(&self, dir: &str, filename: &str) -> Option<(i64, String)> {
        let want_dir = normalize_path(dir);
        let name = filename.to_lowercase();
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.filename.is_empty() {
                continue;
            }
            if state.filename.to_lowercase() == name
                && normalize_path(&state.dir.to_string_lossy()) == want_dir
            {
                return Some((state.id, state.status.clone()));
            }
        }
        None
    }

    async fn unique_filename(&self, dir: &str, base: &str) -> String {
        for number in 1..=9999u32 {
            let candidate = fdm_core::numbered_filename(base, number);
            if self.find_name_conflict(dir, &candidate).await.is_none()
                && !Path::new(dir).join(&candidate).exists()
            {
                return candidate;
            }
        }
        fdm_core::numbered_filename(base, 10000)
    }

    async fn activate_existing(self: &Arc<Self>, existing: &TaskSnapshot) {
        let Some(entry) = self
            .tasks
            .read()
            .ok()
            .and_then(|m| m.get(&existing.id).cloned())
        else {
            return;
        };
        let (status, filepath) = {
            let state = entry.state.lock().await;
            (state.status.clone(), state.filepath.clone())
        };
        match status.as_str() {
            STATUS_DONE => {
                if !filepath.exists() {
                    {
                        let mut state = entry.state.lock().await;
                        state.status = STATUS_PAUSED.to_string();
                        state.downloaded = 0;
                        state.progress = 0.0;
                        state.segments.clear();
                        state.error = None;
                        state.save_cfg();
                    }
                    let _ = self.resume_task(existing.id).await;
                }
            }
            STATUS_PAUSED | STATUS_ERROR | STATUS_QUEUED => {
                let _ = self.resume_task(existing.id).await;
            }
            _ => {}
        }
    }

    pub async fn start_task(self: &Arc<Self>, entry: Arc<TaskEntry>) {
        if !entry.mark_running() {
            return;
        }
        let token = entry.reset_cancel_token();
        {
            let mut state = entry.state.lock().await;
            state.status = STATUS_CONNECTING.to_string();
            state.speed = 0.0;
            state.error = None;
        }
        self.emit_entry(&entry).await;
        let kind = entry.state.lock().await.kind.clone();
        let manager = self.clone();
        match kind.as_str() {
            "bt" => {
                tokio::spawn(async move {
                    manager.run_bt_entry(entry, token).await;
                });
            }
            "ed2k" => {
                tokio::spawn(async move {
                    manager.run_ed2k_entry(entry, token).await;
                });
            }
            _ => {
                tokio::spawn(async move {
                    manager.run_entry(entry, token).await;
                });
            }
        }
    }

    pub async fn pause_task(&self, id: i64) -> bool {
        let Some(entry) = self.tasks.read().ok().and_then(|m| m.get(&id).cloned()) else {
            return false;
        };
        let kind = entry.state.lock().await.kind.clone();
        // 先取消轮询令牌，再暂停底层引擎。否则 BT/ED2K 的进度回写可能
        // 在 engine.pause 完成前继续进行，表现为“已暂停但进度还在涨”。
        entry.request_cancel();
        match kind.as_str() {
            "bt" => {
                if let Some(engine) = self.bt_engine() {
                    if !engine.pause(id).await {
                        tracing::warn!(task_id = id, "暂停 BT 任务失败");
                    }
                }
            }
            "ed2k" => {
                if let Some(engine) = self.ed2k_engine() {
                    if !engine.pause(id).await {
                        tracing::warn!(task_id = id, "暂停 ED2K 任务失败");
                    }
                }
            }
            _ => {}
        }
        {
            let mut state = entry.state.lock().await;
            state.status = STATUS_PAUSED.to_string();
            state.speed = 0.0;
            state.save_cfg();
        }
        self.emit_entry(&entry).await;
        let _ = self.save_tasks().await;
        true
    }

    pub async fn resume_task(self: &Arc<Self>, id: i64) -> bool {
        let Some(entry) = self.tasks.read().ok().and_then(|m| m.get(&id).cloned()) else {
            return false;
        };
        if entry.is_running() {
            {
                let mut state = entry.state.lock().await;
                state.status = STATUS_QUEUED.to_string();
                state.error = None;
            }
            self.emit_entry(&entry).await;
            let _ = self.save_tasks().await;
            return true;
        }
        let kind = entry.state.lock().await.kind.clone();
        if kind != "http" {
            {
                let mut state = entry.state.lock().await;
                state.error = None;
            }
            self.start_task(entry).await;
            let _ = self.save_tasks().await;
            return true;
        }
        let max_concurrent = self.settings.get().max_concurrent.max(1) as usize;
        if self.active_count().await < max_concurrent {
            self.start_task(entry).await;
        } else {
            {
                let mut state = entry.state.lock().await;
                state.status = STATUS_QUEUED.to_string();
                state.error = None;
            }
            self.emit_entry(&entry).await;
        }
        let _ = self.save_tasks().await;
        true
    }

    /// 判断待删除路径是否仍被其他任务引用（重复下载的“覆盖”模式可能共享路径）。
    async fn removal_paths_shared(
        &self,
        part_path: &Path,
        cfg_path: &Path,
        filepath: &Path,
    ) -> (bool, bool, bool) {
        let mut part_shared = false;
        let mut cfg_shared = false;
        let mut file_shared = false;
        for entry in self.entries() {
            let state = entry.state.lock().await;
            // 只有未完成的 HTTP 任务才会继续使用 .fdmpart/.fdmcfg；
            // 已完成任务虽然保留了相同路径字符串，但不能再阻止清理。
            let may_use_temp = state.kind == "http" && state.status != STATUS_DONE;
            if may_use_temp
                && !part_shared
                && !part_path.as_os_str().is_empty()
                && !state.part_path.as_os_str().is_empty()
                && same_path(&state.part_path, part_path)
            {
                part_shared = true;
            }
            if may_use_temp
                && !cfg_shared
                && !cfg_path.as_os_str().is_empty()
                && !state.cfg_path.as_os_str().is_empty()
                && same_path(&state.cfg_path, cfg_path)
            {
                cfg_shared = true;
            }
            if !file_shared
                && !filepath.as_os_str().is_empty()
                && !state.filepath.as_os_str().is_empty()
                && same_path(&state.filepath, filepath)
            {
                file_shared = true;
            }
            if part_shared && cfg_shared && file_shared {
                break;
            }
        }
        (part_shared, cfg_shared, file_shared)
    }

    pub async fn remove_task(&self, id: i64, delete_file: bool) -> bool {
        self.remove_task_inner(id, delete_file, true).await
    }

    async fn remove_task_inner(&self, id: i64, delete_file: bool, save: bool) -> bool {
        let entry = self.tasks.write().ok().and_then(|mut m| m.remove(&id));
        let Some(entry) = entry else {
            return false;
        };
        entry.request_cancel();
        wait_task_stopped(&entry, REMOVE_STOP_TIMEOUT).await;
        let (kind, filename, filepath, part_path, cfg_path) = {
            let state = entry.state.lock().await;
            (
                state.kind.clone(),
                state.filename.clone(),
                state.filepath.clone(),
                state.part_path.clone(),
                state.cfg_path.clone(),
            )
        };
        let (part_shared, cfg_shared, file_shared) = self
            .removal_paths_shared(&part_path, &cfg_path, &filepath)
            .await;
        match kind.as_str() {
            "bt" => {
                if let Some(engine) = self.bt_engine() {
                    if delete_file && file_shared {
                        tracing::warn!(path = %filepath.display(), "BT 文件仍被其他任务引用，跳过删除");
                    }
                    let _ = engine.remove(id, delete_file && !file_shared).await;
                }
            }
            "ed2k" => {
                if let Some(engine) = self.ed2k_engine() {
                    if delete_file && file_shared {
                        tracing::warn!(path = %filepath.display(), "ED2K 文件仍被其他任务引用，跳过删除");
                    }
                    let _ = engine
                        .remove(id, delete_file && !file_shared, &filename)
                        .await;
                }
                // 已完成并搬运到目标目录的任务，删除时也要清掉目标文件。
                if delete_file && !file_shared {
                    remove_file_with_retry(&filepath, "ED2K 目标文件").await;
                }
            }
            _ => {
                // `.fdmpart` / `.fdmcfg` 是未完成下载的临时数据，无论是否保留
                // 最终文件都必须清理；否则“从列表移除”会留下与文件等大的孤儿文件。
                if part_shared {
                    tracing::warn!(path = %part_path.display(), "临时文件仍被其他任务引用，跳过删除");
                } else {
                    remove_file_with_retry(&part_path, "下载临时文件").await;
                }
                if cfg_shared {
                    tracing::warn!(path = %cfg_path.display(), "断点配置仍被其他任务引用，跳过删除");
                } else {
                    remove_file_with_retry(&cfg_path, "断点配置").await;
                }
                if delete_file {
                    if file_shared {
                        tracing::warn!(path = %filepath.display(), "目标文件仍被其他任务引用，跳过删除");
                    } else {
                        remove_file_with_retry(&filepath, "下载文件").await;
                    }
                }
            }
        }
        if delete_file && matches!(kind.as_str(), "bt" | "ed2k") {
            if !part_shared {
                remove_file_with_retry(&part_path, "P2P 临时文件").await;
            }
            if !cfg_shared {
                remove_file_with_retry(&cfg_path, "P2P 断点配置").await;
            }
        }
        if save {
            let _ = self.save_tasks().await;
        }
        self.broadcast("task:remove", serde_json::json!({ "id": id }));
        true
    }

    pub async fn remove_all(&self, delete_file: bool) -> bool {
        let ids = self
            .tasks
            .read()
            .map(|m| m.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        futures_util::future::join_all(
            ids.into_iter()
                .map(|id| self.remove_task_inner(id, delete_file, false)),
        )
        .await;
        let _ = self.save_tasks().await;
        true
    }

    pub async fn set_global_rate(&self, kb_per_sec: u64) {
        let rate = kb_per_sec.saturating_mul(1024);
        for entry in self.entries() {
            let mut state = entry.state.lock().await;
            if !state.rate_explicit {
                state.max_rate = rate;
                entry.limiter.set_rate(rate);
            }
        }
        if let Some(engine) = self.bt_engine() {
            engine.set_global_rate(kb_per_sec).await;
        }
        if let Some(engine) = self.ed2k_engine() {
            engine.set_global_rate(kb_per_sec).await;
        }
        let _ = self.save_tasks().await;
    }

    /// 默认下载目录变化时同步给 ED2K 引擎；BT 在下次添加任务时直接使用新目录。
    pub async fn apply_default_dir(&self, dir: &str) {
        let Some(engine) = self.ed2k_engine() else {
            return;
        };
        let mut has_active = false;
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.kind == "ed2k" && is_active_status(&state.status) {
                has_active = true;
                break;
            }
        }
        if !has_active {
            let _ = engine.set_download_dir(Path::new(dir)).await;
        }
    }

    pub fn start_scheduler(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_millis(700)).await;
                manager.schedule_once().await;
            }
        });
    }

    async fn schedule_once(self: &Arc<Self>) {
        // BT/ED2K 由各自引擎调度，不受 HTTP 并发上限约束。
        let mut p2p_queued = Vec::new();
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.status == STATUS_QUEUED && !entry.is_running() && state.kind != "http" {
                p2p_queued.push(entry.clone());
            }
        }
        for entry in p2p_queued {
            self.start_task(entry).await;
        }
        let max_concurrent = self.settings.get().max_concurrent.max(1) as usize;
        let mut active = self.active_count().await;
        if active >= max_concurrent {
            return;
        }
        let mut queued = Vec::new();
        for entry in self.entries() {
            let state = entry.state.lock().await;
            if state.status == STATUS_QUEUED && !entry.is_running() {
                queued.push(entry.clone());
            }
        }
        queued.sort_by_key(|e| e.id);
        for entry in queued {
            if active >= max_concurrent {
                break;
            }
            self.start_task(entry).await;
            active += 1;
        }
    }

    async fn run_bt_entry(self: Arc<Self>, entry: Arc<TaskEntry>, token: CancellationToken) {
        let result = self.run_bt_entry_inner(&entry, &token).await;
        if token.is_cancelled() {
            let mut state = entry.state.lock().await;
            if state.status != STATUS_DONE && state.status != STATUS_SEEDING {
                state.status = STATUS_PAUSED.to_string();
            }
            state.speed = 0.0;
        } else if let Err(err) = result {
            let mut state = entry.state.lock().await;
            state.status = STATUS_ERROR.to_string();
            state.error = Some(err.to_string());
            state.speed = 0.0;
        }
        let _ = self.save_tasks().await;
        self.emit_entry(&entry).await;
        entry.mark_stopped();
    }

    async fn run_bt_entry_inner(
        self: &Arc<Self>,
        entry: &Arc<TaskEntry>,
        token: &CancellationToken,
    ) -> Result<()> {
        let engine = self.bt_engine().ok_or_else(|| anyhow!("BT 引擎未启用"))?;
        let (id, url, torrent_path, dir, info_hash) = {
            let state = entry.state.lock().await;
            (
                state.id,
                state.url.clone(),
                state.torrent_path.clone(),
                state.dir.clone(),
                state.info_hash.clone(),
            )
        };
        {
            let mut state = entry.state.lock().await;
            state.status = STATUS_CONNECTING.to_string();
            state.error = None;
            state.speed = 0.0;
        }
        self.emit_entry(entry).await;

        let already = match info_hash.as_deref() {
            Some(hash) => engine.reattach(id, hash).await,
            None => false,
        };
        if already {
            let _ = engine.resume(id).await;
        } else {
            let source = if let Some(path) = torrent_path.filter(|p| Path::new(p).exists()) {
                BtSource::TorrentBytes(std::fs::read(&path).context("读取缓存的种子文件失败")?)
            } else if url.starts_with("magnet:") {
                BtSource::Magnet(url)
            } else if url.starts_with("http://") || url.starts_with("https://") {
                let client = self.http_client()?;
                let settings = self.settings.get();
                let candidates = github_proxy_candidates(&url, &settings, false);
                BtSource::TorrentBytes(
                    fetch_torrent_bytes_candidates(client.as_ref(), &candidates)
                        .await
                        .context("下载种子文件失败")?,
                )
            } else {
                bail!("BT 任务缺少磁力链接或种子文件");
            };
            let info = engine.add(id, source, &dir, false, None).await?;
            let mut state = entry.state.lock().await;
            state.info_hash = Some(info.info_hash.clone());
            // 种子名来自远端元数据，统一清洗后再用于展示与路径。
            state.filename = fdm_core::sanitize_filename(&info.name);
            state.files = info.files.clone();
            state.total = Some(info.total);
            state.dir = info.output_dir.clone();
            state.filepath = info.output_dir.clone();
            state.status = STATUS_DOWNLOADING.to_string();
            state.downloaded = 0;
            state.progress = 0.0;
            drop(state);
            self.save_tasks().await?;
            self.emit_entry(entry).await;
        }

        loop {
            if token.is_cancelled() {
                let _ = engine.pause(id).await;
                return Ok(());
            }
            let Some(stats) = engine.stats(id).await else {
                bail!("BT 任务状态丢失");
            };
            let mut stop = false;
            {
                let mut state = entry.state.lock().await;
                state.downloaded = stats.progress_bytes;
                if stats.total_bytes > 0 {
                    state.total = Some(stats.total_bytes);
                }
                state.speed = stats.download_speed as f64;
                state.uploaded = stats.uploaded_bytes;
                state.upload_speed = stats.upload_speed as f64;
                state.peers = stats.peers;
                for (index, progress) in stats.files.iter().enumerate() {
                    if let Some(file) = state.files.get_mut(index) {
                        file.progress = *progress;
                    }
                }
                if let Some(total) = state.total {
                    state.progress = if total > 0 {
                        ((stats.progress_bytes as f64 / total as f64) * 10000.0).round() / 10000.0
                    } else {
                        0.0
                    };
                }
                match stats.state {
                    Some(BtState::Error) => {
                        state.status = STATUS_ERROR.to_string();
                        state.error = stats.error.clone().or_else(|| Some("BT 下载出错".into()));
                        state.speed = 0.0;
                        stop = true;
                    }
                    Some(BtState::Finished) => {
                        let seed = self.settings.get().bt.seed_after_complete;
                        state.status = if seed { STATUS_SEEDING } else { STATUS_DONE }.to_string();
                        state.progress = 1.0;
                        if let Some(total) = state.total {
                            state.downloaded = total;
                        }
                        stop = true;
                    }
                    Some(BtState::Paused) => {
                        state.status = STATUS_PAUSED.to_string();
                        state.speed = 0.0;
                        stop = true;
                    }
                    _ => {
                        state.status = STATUS_DOWNLOADING.to_string();
                    }
                }
            }
            self.emit_entry(entry).await;
            if stop {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let (status, seed) = {
            let state = entry.state.lock().await;
            (
                state.status.clone(),
                self.settings.get().bt.seed_after_complete,
            )
        };
        if status == STATUS_DONE && !seed {
            let _ = engine.pause(id).await;
        }
        Ok(())
    }

    async fn run_ed2k_entry(self: Arc<Self>, entry: Arc<TaskEntry>, token: CancellationToken) {
        let result = self.run_ed2k_entry_inner(&entry, &token).await;
        if token.is_cancelled() {
            let mut state = entry.state.lock().await;
            if state.status != STATUS_DONE && state.status != STATUS_SEEDING {
                state.status = STATUS_PAUSED.to_string();
            }
            state.speed = 0.0;
        } else if let Err(err) = result {
            let mut state = entry.state.lock().await;
            state.status = STATUS_ERROR.to_string();
            state.error = Some(err.to_string());
            state.speed = 0.0;
        }
        let _ = self.save_tasks().await;
        self.emit_entry(&entry).await;
        entry.mark_stopped();
    }

    async fn run_ed2k_entry_inner(
        self: &Arc<Self>,
        entry: &Arc<TaskEntry>,
        token: &CancellationToken,
    ) -> Result<()> {
        let engine = self
            .ed2k_engine()
            .ok_or_else(|| anyhow!("ED2K 引擎未启用"))?;
        let (id, url, known_hash) = {
            let state = entry.state.lock().await;
            (state.id, state.url.clone(), state.ed2k_hash.clone())
        };
        {
            let mut state = entry.state.lock().await;
            state.status = STATUS_CONNECTING.to_string();
            state.error = None;
            state.speed = 0.0;
        }
        self.emit_entry(entry).await;

        let mut already = false;
        if engine.stats(id).await.is_some() {
            already = true;
        } else if let Some(hash) = known_hash.as_deref() {
            already = engine.attach(id, hash).await;
        }
        if already {
            let _ = engine.resume(id).await;
        } else {
            if !url.starts_with("ed2k://") {
                bail!("ED2K 任务缺少 ed2k 链接");
            }
            let hash = engine.add(id, &url).await?;
            let mut state = entry.state.lock().await;
            state.ed2k_hash = Some(hash);
            state.status = STATUS_DOWNLOADING.to_string();
            drop(state);
            self.save_tasks().await?;
            self.emit_entry(entry).await;
        }

        loop {
            if token.is_cancelled() {
                let _ = engine.pause(id).await;
                return Ok(());
            }
            let Some(stats) = engine.stats(id).await else {
                bail!("ED2K 任务状态丢失");
            };
            let mut stop = false;
            {
                let mut state = entry.state.lock().await;
                if let Some(name) = stats.name.clone() {
                    // aMule 返回的文件名不可信，清洗后再用于展示与目标路径。
                    state.filename = fdm_core::sanitize_filename(&name);
                }
                if stats.size > 0 {
                    state.total = Some(stats.size);
                }
                state.downloaded = stats.completed;
                state.speed = stats.speed as f64;
                state.sources = stats.sources;
                if let Some(total) = state.total {
                    const PART: u64 = 9_280_000;
                    let mut segments = Vec::new();
                    for (index, part) in stats.parts.iter().enumerate() {
                        let start = index as u64 * PART;
                        if start >= total {
                            break;
                        }
                        let end = (start + PART).min(total) - 1;
                        let mut segment = Segment::new(index, start, end);
                        if part.state == "complete" {
                            segment.cur = end + 1;
                            segment.done = true;
                        }
                        segments.push(segment);
                    }
                    state.segments = segments;
                }
                state.uploaded = stats.uploaded;
                state.upload_speed = stats.upload_speed as f64;
                if let Some(total) = state.total {
                    state.progress = if total > 0 {
                        ((stats.completed as f64 / total as f64) * 10000.0).round() / 10000.0
                    } else {
                        0.0
                    };
                }
                match stats.state {
                    Some(Ed2kState::Error) => {
                        state.status = STATUS_ERROR.to_string();
                        state.error = stats.error.clone().or_else(|| Some("ED2K 下载出错".into()));
                        state.speed = 0.0;
                        stop = true;
                    }
                    Some(Ed2kState::Completed) => {
                        let share = self.settings.get().ed2k.share_completed;
                        if !share {
                            let _ = engine.clear_completed(id).await;
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            let dir = state.dir.clone();
                            let name = state.filename.clone();
                            let target = Path::new(&dir).join(&name);
                            if engine.finalize_file(&name, &target) {
                                state.filepath = target;
                            }
                            state.status = STATUS_DONE.to_string();
                        } else {
                            state.status = STATUS_SEEDING.to_string();
                        }
                        state.progress = 1.0;
                        if let Some(total) = state.total {
                            state.downloaded = total;
                        }
                        stop = true;
                    }
                    Some(Ed2kState::Paused) => {
                        state.status = STATUS_PAUSED.to_string();
                        state.speed = 0.0;
                        stop = true;
                    }
                    Some(Ed2kState::Waiting) => {
                        state.status = STATUS_WAITING.to_string();
                        state.speed = 0.0;
                    }
                    Some(Ed2kState::Hashing) => {
                        state.status = STATUS_CONNECTING.to_string();
                    }
                    _ => {
                        state.status = STATUS_DOWNLOADING.to_string();
                    }
                }
            }
            self.emit_entry(entry).await;
            if stop {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    }

    /// 重启后把 BT/ED2K 引擎里的状态重新挂到任务列表。
    async fn reconcile_p2p(self: &Arc<Self>) {
        let entries = self.entries();
        for entry in entries {
            let (id, kind, info_hash, ed2k_hash) = {
                let state = entry.state.lock().await;
                (
                    state.id,
                    state.kind.clone(),
                    state.info_hash.clone(),
                    state.ed2k_hash.clone(),
                )
            };
            if kind == "bt" {
                let Some(engine) = self.bt_engine() else {
                    continue;
                };
                let Some(hash) = info_hash else {
                    continue;
                };
                if engine.reattach(id, &hash).await {
                    // 启动时 librqbit 会自动挂载持久化的 torrent。若任务保存为
                    // 暂停，必须同时暂停引擎，否则它会继续后台下载，而界面仍
                    // 显示“已暂停”和旧进度。
                    let before = engine.stats(id).await;
                    let finished = matches!(
                        before.as_ref().and_then(|s| s.state),
                        Some(BtState::Finished)
                    );
                    let seed = self.settings.get().bt.seed_after_complete;
                    if finished && seed {
                        let _ = engine.resume(id).await;
                    } else if !engine.pause(id).await {
                        tracing::warn!(task_id = id, "恢复 BT 任务后暂停引擎失败");
                    }
                    if let Some(stats) = engine.stats(id).await.or(before) {
                        let mut state = entry.state.lock().await;
                        state.downloaded = stats.progress_bytes;
                        state.uploaded = stats.uploaded_bytes;
                        if stats.total_bytes > 0 {
                            state.total = Some(stats.total_bytes);
                        }
                        if let Some(total) = state.total {
                            state.progress = if total > 0 {
                                ((stats.progress_bytes as f64 / total as f64) * 10000.0).round()
                                    / 10000.0
                            } else {
                                0.0
                            };
                        }
                        state.status = match stats.state {
                            Some(BtState::Finished) => {
                                if self.settings.get().bt.seed_after_complete {
                                    STATUS_SEEDING
                                } else {
                                    STATUS_DONE
                                }
                            }
                            Some(BtState::Error) => STATUS_ERROR,
                            _ => STATUS_PAUSED,
                        }
                        .to_string();
                    }
                } else {
                    entry.state.lock().await.status = STATUS_PAUSED.to_string();
                }
            } else if kind == "ed2k" {
                let Some(engine) = self.ed2k_engine() else {
                    continue;
                };
                let Some(hash) = ed2k_hash else {
                    continue;
                };
                if engine.attach(id, &hash).await {
                    // 同上：aMule 会保留未完成任务并继续下载，启动时必须
                    // 显式暂停，保证任务列表的“已暂停”与实际一致。
                    let before = engine.stats(id).await;
                    let completed = matches!(
                        before.as_ref().and_then(|s| s.state),
                        Some(Ed2kState::Completed)
                    );
                    if !completed && !engine.pause(id).await {
                        tracing::warn!(task_id = id, "恢复 ED2K 任务后暂停引擎失败");
                    }
                    if let Some(stats) = engine.stats(id).await.or(before) {
                        let mut state = entry.state.lock().await;
                        state.downloaded = stats.completed;
                        if stats.size > 0 {
                            state.total = Some(stats.size);
                        }
                        if let Some(total) = state.total {
                            state.progress = if total > 0 {
                                ((stats.completed as f64 / total as f64) * 10000.0).round()
                                    / 10000.0
                            } else {
                                0.0
                            };
                        }
                        state.status = match stats.state {
                            Some(Ed2kState::Completed) => STATUS_DONE,
                            Some(Ed2kState::Error) => STATUS_ERROR,
                            _ => STATUS_PAUSED,
                        }
                        .to_string();
                    }
                } else {
                    entry.state.lock().await.status = STATUS_PAUSED.to_string();
                }
            }
        }
        let _ = self.save_tasks().await;
    }

    /// 应用退出前终止 BT 会话与 aMule 子进程。
    pub async fn shutdown_engines(&self) {
        if let Some(engine) = self.bt_engine() {
            engine.shutdown().await;
        }
        if let Some(engine) = self.ed2k_engine() {
            engine.shutdown().await;
        }
    }

    async fn run_entry(self: Arc<Self>, entry: Arc<TaskEntry>, token: CancellationToken) {
        let result = self.run_entry_inner(&entry, &token).await;
        if token.is_cancelled() {
            let mut state = entry.state.lock().await;
            if state.status != STATUS_PAUSED && state.status != STATUS_QUEUED {
                state.status = STATUS_PAUSED.to_string();
            }
            state.speed = 0.0;
            state.save_cfg();
            drop(state);
        } else if let Err(err) = result {
            let mut state = entry.state.lock().await;
            state.status = STATUS_ERROR.to_string();
            state.error = Some(err.to_string());
            state.speed = 0.0;
            drop(state);
        }
        let _ = self.save_tasks().await;
        self.emit_entry(&entry).await;
        entry.mark_stopped();
    }

    /// 计算任务当前 `.fdmpart` 的规范化路径键。
    async fn part_key(entry: &Arc<TaskEntry>) -> Option<String> {
        let state = entry.state.lock().await;
        if state.part_path.as_os_str().is_empty() {
            None
        } else {
            Some(normalize_path(state.part_path.to_string_lossy().as_ref()))
        }
    }

    /// 尝试独占某个 `.fdmpart` 的写入权；返回 None 表示已被其他任务占用。
    fn try_claim_part(self: &Arc<Self>, key: &str) -> Option<PartClaim> {
        let inserted = match self.part_claims.lock() {
            Ok(mut claims) => claims.insert(key.to_string()),
            Err(poisoned) => poisoned.into_inner().insert(key.to_string()),
        };
        if inserted {
            Some(PartClaim {
                manager: self.clone(),
                key: key.to_string(),
            })
        } else {
            None
        }
    }

    async fn run_entry_inner(
        self: &Arc<Self>,
        entry: &Arc<TaskEntry>,
        token: &CancellationToken,
    ) -> Result<()> {
        let had_cfg = { entry.state.lock().await.load_cfg() };
        if had_cfg {
            let (complete, part_exists, file_ok) = {
                let state = entry.state.lock().await;
                let file_ok = state
                    .total
                    .map(|total| {
                        fs::metadata(&state.filepath)
                            .map(|m| m.len() >= total)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                (state.segments_complete(), state.part_path.exists(), file_ok)
            };
            if complete {
                if part_exists {
                    self.finalize_complete(entry, true).await?;
                    return Ok(());
                }
                if file_ok {
                    self.finalize_complete(entry, false).await?;
                    return Ok(());
                }
                entry.state.lock().await.segments.clear();
            }
        }
        if token.is_cancelled() {
            return Ok(());
        }
        {
            let mut state = entry.state.lock().await;
            state.status = STATUS_CONNECTING.to_string();
            state.speed = 0.0;
            state.error = None;
        }
        self.emit_entry(entry).await;

        let client = self.http_client().context("创建 HTTP 客户端失败")?;
        let need_probe = {
            let state = entry.state.lock().await;
            !(had_cfg
                && state.total.is_some()
                && !state.segments.is_empty()
                && state.part_path.exists())
        };
        let mut part_claim: Option<PartClaim> = None;
        let mut create_part: Option<(PathBuf, Option<u64>)> = None;
        let mut persist_layout = false;
        let (url, filename, headers) = {
            let state = entry.state.lock().await;
            (
                state.url.clone(),
                state.filename.clone(),
                state.custom_headers.clone(),
            )
        };
        let settings = self.settings.get();
        let candidates = Arc::new(github_proxy_candidates(
            &url,
            &settings,
            has_sensitive_headers(&headers),
        ));
        let mut candidate_index_value = 0usize;
        if need_probe {
            let (result, used_index) =
                probe_candidates(client.as_ref(), candidates.as_slice(), &headers, &filename)
                    .await
                    .context("探测下载地址失败")?;
            candidate_index_value = used_index;
            let supports_range = result.supports_range;
            let (part_path, prealloc_total, layout_persist) = {
                let mut state = entry.state.lock().await;
                state.filename = result.filename;
                state.total = result.total.or(state.total);
                state.setup_paths();
                state.segments.clear();
                state.downloaded = 0;
                state.progress = 0.0;
                fdm_core::paths::ensure_dir(&state.dir)?;
                let total = state.total;
                let connections = state.connections;
                let part_path = state.part_path.clone();
                if supports_range && total.unwrap_or(0) > 0 && connections > 1 {
                    let total = total.unwrap();
                    let count = (connections as u64).min(total).max(1) as usize;
                    let part_size = total / count as u64;
                    let mut segments = Vec::with_capacity(count);
                    for i in 0..count {
                        let start = i as u64 * part_size;
                        let end = if i < count - 1 {
                            (i as u64 + 1) * part_size - 1
                        } else {
                            total - 1
                        };
                        segments.push(Segment::new(i, start, end));
                    }
                    state.segments = segments;
                    (part_path, Some(total), true)
                } else {
                    let end = total.map(|t| t.saturating_sub(1)).unwrap_or(0);
                    state.segments = vec![Segment::new(0, 0, end)];
                    (part_path, None, false)
                }
            };
            create_part = Some((part_path, prealloc_total));
            persist_layout = layout_persist;
        }

        // 同一个 .fdmpart 只允许一个任务写入：冲突时重新排队，等前一个任务结束后由调度器重试。
        if let Some(key) = Self::part_key(entry).await {
            match self.try_claim_part(&key) {
                Some(claim) => part_claim = Some(claim),
                None => {
                    {
                        let mut state = entry.state.lock().await;
                        // 用户可能在探测期间已暂停/取消，不能把它误改回排队。
                        state.status = if token.is_cancelled() {
                            STATUS_PAUSED.to_string()
                        } else {
                            STATUS_QUEUED.to_string()
                        };
                        state.speed = 0.0;
                    }
                    self.emit_entry(entry).await;
                    return Ok(());
                }
            }
        }
        if token.is_cancelled() {
            return Ok(());
        }
        // 预分配/建文件放到锁外，且必须在拿到写入权之后，避免并发截断同一文件。
        if let Some((part_path, prealloc_total)) = create_part {
            if let Some(total) = prealloc_total {
                AsyncFile::create(&part_path).await?.set_len(total).await?;
            } else {
                AsyncOpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part_path)
                    .await?;
            }
        }
        if persist_layout {
            entry.state.lock().await.save_cfg();
        }
        if token.is_cancelled() {
            return Ok(());
        }

        let candidate_index = Arc::new(AtomicUsize::new(candidate_index_value));
        let multi = {
            let mut state = entry.state.lock().await;
            state.status = STATUS_DOWNLOADING.to_string();
            state.speed = 0.0;
            state.error = None;
            state.save_cfg();
            state.segments.len() > 1 && state.total.is_some()
        };
        self.emit_entry(entry).await;

        let speed_manager = self.clone();
        let speed_entry = entry.clone();
        let speed_token = token.clone();
        let speed_handle = tokio::spawn(async move {
            speed_manager.speed_loop(speed_entry, speed_token).await;
        });

        let download_result = if multi {
            self.run_workers(
                entry.clone(),
                client.clone(),
                token.clone(),
                candidates.clone(),
                candidate_index.clone(),
            )
            .await
        } else {
            self.download_single(
                entry.clone(),
                client.clone(),
                token.clone(),
                candidates.clone(),
                candidate_index.clone(),
            )
            .await
        };
        speed_handle.abort();

        if token.is_cancelled() {
            let mut state = entry.state.lock().await;
            state.status = STATUS_PAUSED.to_string();
            state.speed = 0.0;
            state.save_cfg();
            drop(state);
            self.emit_entry(entry).await;
            return Ok(());
        }
        download_result?;
        let all_done = {
            let mut state = entry.state.lock().await;
            state.refresh_progress();
            if !state.segments.is_empty() {
                state.segments_complete()
            } else if let Some(total) = state.total {
                state.downloaded >= total
            } else {
                state.downloaded > 0
            }
        };
        if !all_done {
            bail!("下载未完成");
        }
        self.finalize_complete(entry, true).await?;
        drop(part_claim);
        Ok(())
    }

    async fn finalize_complete(&self, entry: &Arc<TaskEntry>, merge_part: bool) -> Result<()> {
        let (part_path, filepath, cfg_path, total) = {
            let state = entry.state.lock().await;
            (
                state.part_path.clone(),
                state.filepath.clone(),
                state.cfg_path.clone(),
                state.total,
            )
        };
        if merge_part && part_path.exists() {
            {
                let mut state = entry.state.lock().await;
                state.status = STATUS_MERGING.to_string();
                state.speed = 0.0;
            }
            self.emit_entry(entry).await;
            sleep(Duration::from_millis(200)).await;
            if filepath.exists() {
                let _ = fs::remove_file(&filepath);
            }
            fs::rename(&part_path, &filepath)
                .with_context(|| format!("合并文件失败: {}", filepath.display()))?;
        }
        if cfg_path.exists() {
            let _ = fs::remove_file(&cfg_path);
        }
        let mut state = entry.state.lock().await;
        state.status = STATUS_DONE.to_string();
        state.progress = 1.0;
        state.speed = 0.0;
        state.error = None;
        if let Some(total) = total {
            state.downloaded = total;
        } else if let Ok(meta) = fs::metadata(&state.filepath) {
            state.total = Some(meta.len());
            state.downloaded = meta.len();
        }
        drop(state);
        Ok(())
    }

    async fn speed_loop(self: Arc<Self>, entry: Arc<TaskEntry>, token: CancellationToken) {
        let mut last_time = Instant::now();
        let mut last_bytes = { entry.state.lock().await.downloaded };
        loop {
            tokio::select! {
                _ = sleep(PROGRESS_FLUSH_INTERVAL) => {},
                _ = token.cancelled() => break,
            }
            let now = Instant::now();
            let (current, status) = {
                let mut state = entry.state.lock().await;
                state.refresh_progress();
                let current = state.downloaded;
                let status = state.status.clone();
                if status == STATUS_DOWNLOADING {
                    let dt = now.duration_since(last_time).as_secs_f64();
                    if dt > 0.0 {
                        let instant_speed = current.saturating_sub(last_bytes) as f64 / dt;
                        state.speed = if state.speed == 0.0 {
                            instant_speed
                        } else {
                            state.speed * 0.4 + instant_speed * 0.6
                        };
                    }
                }
                (current, status)
            };
            if status != STATUS_DOWNLOADING {
                break;
            }
            last_bytes = current;
            last_time = now;
            self.emit_entry(&entry).await;
        }
    }

    async fn run_workers(
        self: &Arc<Self>,
        entry: Arc<TaskEntry>,
        client: Arc<reqwest::Client>,
        token: CancellationToken,
        candidates: Arc<Vec<String>>,
        candidate_index: Arc<AtomicUsize>,
    ) -> Result<()> {
        let worker_count = {
            let mut state = entry.state.lock().await;
            for segment in &mut state.segments {
                segment.worker = None;
            }
            state.connections.max(1).min(state.segments.len().max(1))
        };
        let worker_token = token.child_token();
        let mut set = tokio::task::JoinSet::new();
        for worker in 0..worker_count {
            let manager = self.clone();
            let worker_entry = entry.clone();
            let worker_client = client.clone();
            let worker_cancel = worker_token.clone();
            let worker_candidates = candidates.clone();
            let worker_candidate_index = candidate_index.clone();
            set.spawn(async move {
                manager
                    .download_worker(
                        worker_entry,
                        worker,
                        worker_client,
                        worker_cancel,
                        worker_candidates,
                        worker_candidate_index,
                    )
                    .await
            });
        }
        let mut first_error = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                        worker_token.cancel();
                    }
                }
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow!("下载 worker 异常退出: {err}"));
                        worker_token.cancel();
                    }
                }
            }
        }
        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }

    async fn download_worker(
        self: &Arc<Self>,
        entry: Arc<TaskEntry>,
        worker: usize,
        client: Arc<reqwest::Client>,
        token: CancellationToken,
        candidates: Arc<Vec<String>>,
        candidate_index: Arc<AtomicUsize>,
    ) -> Result<()> {
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            let Some(index) = self.claim_segment(&entry, worker).await? else {
                return Ok(());
            };
            self.download_range(
                entry.clone(),
                index,
                client.clone(),
                token.clone(),
                candidates.clone(),
                candidate_index.clone(),
            )
            .await?;
        }
    }

    /// 领取未分配分片；没有空闲分片时，从剩余字节最多的慢段切走一半。
    async fn claim_segment(&self, entry: &Arc<TaskEntry>, worker: usize) -> Result<Option<usize>> {
        let mut state = entry.state.lock().await;
        if let Some(index) = state
            .segments
            .iter()
            .position(|segment| !segment.complete() && segment.worker.is_none())
        {
            if let Some(segment) = state.segments.get_mut(index) {
                segment.worker = Some(worker);
            }
            return Ok(Some(index));
        }

        let connections = state.connections.max(1);
        let max_segments = connections.saturating_mul(MAX_SPLITS_PER_CONNECTION);
        if state.segments.len() >= max_segments {
            return Ok(None);
        }

        let mut victim: Option<(usize, u64)> = None;
        for (index, segment) in state.segments.iter().enumerate() {
            if segment.complete() || segment.cur > segment.end {
                continue;
            }
            let remaining = segment.end.saturating_sub(segment.cur).saturating_add(1);
            if remaining >= MIN_SPLIT_BYTES
                && victim.map(|(_, best)| remaining > best).unwrap_or(true)
            {
                victim = Some((index, remaining));
            }
        }
        let Some((victim_index, remaining)) = victim else {
            return Ok(None);
        };

        let (mid, old_end) = {
            let segment = &mut state.segments[victim_index];
            let mid = segment.cur.saturating_add(remaining / 2);
            if mid <= segment.cur || mid > segment.end {
                return Ok(None);
            }
            let old_end = segment.end;
            segment.end = mid - 1;
            (mid, old_end)
        };
        let new_index = state.segments.len();
        let mut split = Segment::new(new_index, mid, old_end);
        split.worker = Some(worker);
        state.segments.push(split);
        tracing::debug!(
            worker,
            victim = victim_index,
            new_segment = new_index,
            mid,
            old_end,
            "工作窃取：切分慢段"
        );
        Ok(Some(new_index))
    }

    async fn download_range(
        &self,
        entry: Arc<TaskEntry>,
        index: usize,
        client: Arc<reqwest::Client>,
        token: CancellationToken,
        candidates: Arc<Vec<String>>,
        candidate_index: Arc<AtomicUsize>,
    ) -> Result<()> {
        let mut failures = 0_u32;
        'outer: loop {
            if token.is_cancelled() {
                return Ok(());
            }
            let (url, headers, part_path, start, end, cur, done, segment_count, used_index) = {
                let state = entry.state.lock().await;
                let segment = state
                    .segments
                    .get(index)
                    .ok_or_else(|| anyhow!("分块不存在: {}", index))?;
                let used_index = candidate_index.load(Ordering::SeqCst);
                let url = candidates
                    .get(used_index)
                    .cloned()
                    .unwrap_or_else(|| state.url.clone());
                (
                    url,
                    state.custom_headers.clone(),
                    state.part_path.clone(),
                    segment.start,
                    segment.end,
                    segment.cur,
                    segment.complete(),
                    state.segments.len(),
                    used_index,
                )
            };
            if done || cur > end {
                break;
            }

            let mut request_headers = headers_map(&headers);
            request_headers.insert(
                RANGE,
                HeaderValue::from_str(&format!("bytes={}-{}", cur, end))?,
            );
            let resp_result = tokio::select! {
                _ = token.cancelled() => return Ok(()),
                result = client.get(&url).headers(request_headers).send() => result,
            };
            let resp = match resp_result {
                Ok(resp) => resp,
                Err(err) => {
                    if Self::advance_proxy_candidate(
                        &candidate_index,
                        candidates.as_slice(),
                        used_index,
                        "连接失败",
                    ) {
                        failures = 0;
                        continue;
                    }
                    failures += 1;
                    if failures >= 8 {
                        return Err(err.into());
                    }
                    sleep(Duration::from_millis(1500)).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 416 {
                let mut state = entry.state.lock().await;
                if let Some(segment) = state.segments.get_mut(index) {
                    segment.done = true;
                    segment.cur = segment.end.saturating_add(1);
                }
                state.refresh_progress();
                break;
            }
            let invalid_range = status == 200 && (cur > start || segment_count > 1);
            let bad_status = status != 206 && status != 200;
            let bad_content_range = status == 206 && Self::content_range_mismatch(&resp, cur);
            if invalid_range || bad_status || bad_content_range {
                if Self::advance_proxy_candidate(
                    &candidate_index,
                    candidates.as_slice(),
                    used_index,
                    "分片响应异常",
                ) {
                    failures = 0;
                    continue;
                }
                if bad_status {
                    failures += 1;
                    if failures >= 8 {
                        return Err(anyhow!("HTTP {}", status));
                    }
                    sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                return Err(anyhow!(
                    "服务器忽略了 Range 请求或返回了错误的 Content-Range，分块 #{}",
                    index
                ));
            }
            failures = 0;

            let file = AsyncOpenOptions::new().write(true).open(&part_path).await?;
            let mut writer = BufferedWriter::new(file, cur).await?;
            let mut known_end = end;
            let mut stream = resp.bytes_stream();
            let mut stream_error = None;
            let mut cancelled = false;
            loop {
                let next = tokio::select! {
                    _ = token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    item = stream.next() => item,
                };
                let Some(chunk) = next else {
                    break;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        stream_error = Some(err);
                        break;
                    }
                };
                if chunk.is_empty() {
                    continue;
                }
                if writer.offset() > known_end {
                    break;
                }
                let remaining = known_end.saturating_sub(writer.offset()).saturating_add(1);
                let take = chunk.len().min(remaining as usize);
                if take == 0 {
                    break;
                }
                if !entry.limiter.acquire(take, &token).await {
                    cancelled = true;
                    break;
                }
                writer.pending.extend_from_slice(&chunk[..take]);
                if writer.should_flush() {
                    let (_, end_exclusive) =
                        Self::flush_segment(&entry, index, &mut writer).await?;
                    known_end = end_exclusive.saturating_sub(1);
                }
            }

            // 正常结束、取消、错误或分片被切小时都必须先把缓冲写盘并同步进度。
            Self::flush_segment(&entry, index, &mut writer).await?;

            if cancelled {
                return Ok(());
            }
            if let Some(err) = stream_error {
                if failures >= 1 {
                    if Self::advance_proxy_candidate(
                        &candidate_index,
                        candidates.as_slice(),
                        used_index,
                        "下载流中断",
                    ) {
                        failures = 0;
                        continue 'outer;
                    }
                }
                failures += 1;
                if failures >= 8 {
                    return Err(err.into());
                }
                sleep(Duration::from_millis(1500)).await;
                continue 'outer;
            }
            let complete = {
                entry
                    .state
                    .lock()
                    .await
                    .segments
                    .get(index)
                    .map(Segment::complete)
                    .unwrap_or(false)
            };
            if complete {
                break;
            }
            failures += 1;
            if failures >= 8 {
                return Err(anyhow!("分块 #{} 多次重试仍未完成", index));
            }
            sleep(Duration::from_millis(1500)).await;
        }
        Ok(())
    }

    async fn flush_segment(
        entry: &Arc<TaskEntry>,
        index: usize,
        writer: &mut BufferedWriter,
    ) -> Result<(u64, u64)> {
        let end_exclusive = {
            let state = entry.state.lock().await;
            let segment = state
                .segments
                .get(index)
                .ok_or_else(|| anyhow!("分块不存在: {}", index))?;
            segment.end.saturating_add(1)
        };
        let offset = writer.flush_limit(end_exclusive).await?;
        let cfg_payload = {
            let mut state = entry.state.lock().await;
            if let Some(segment) = state.segments.get_mut(index) {
                segment.cur = offset;
                if offset > segment.end {
                    segment.done = true;
                }
            }
            state.refresh_progress();
            let due = entry
                .cfg_saved_at
                .lock()
                .map(|mut saved_at| {
                    if saved_at.elapsed() >= CFG_SAVE_INTERVAL {
                        *saved_at = Instant::now();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if due {
                state.cfg_payload()
            } else {
                None
            }
        };
        // worker 在结束前等待落盘，run_workers 汇合后才会 finalize，不会与删 cfg 竞态。
        if let Some((cfg_path, bytes)) = cfg_payload {
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(err) = atomic_write(&cfg_path, &bytes) {
                    tracing::warn!(error = %err, path = %cfg_path.display(), "保存断点配置失败");
                }
            })
            .await;
        }
        Ok((offset, end_exclusive))
    }

    async fn download_single(
        &self,
        entry: Arc<TaskEntry>,
        client: Arc<reqwest::Client>,
        token: CancellationToken,
        candidates: Arc<Vec<String>>,
        candidate_index: Arc<AtomicUsize>,
    ) -> Result<()> {
        let mut failures = 0_u32;
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            let (url, headers, part_path, cur, total, used_index) = {
                let state = entry.state.lock().await;
                let used_index = candidate_index.load(Ordering::SeqCst);
                let url = candidates
                    .get(used_index)
                    .cloned()
                    .unwrap_or_else(|| state.url.clone());
                (
                    url,
                    state.custom_headers.clone(),
                    state.part_path.clone(),
                    state.downloaded,
                    state.total,
                    used_index,
                )
            };
            if let Some(total) = total {
                if cur >= total {
                    break;
                }
            }
            let mut request_headers = headers_map(&headers);
            if cur > 0 {
                request_headers.insert(RANGE, HeaderValue::from_str(&format!("bytes={}-", cur))?);
            }
            let resp_result = tokio::select! {
                _ = token.cancelled() => return Ok(()),
                result = client.get(&url).headers(request_headers).send() => result,
            };
            let resp = match resp_result {
                Ok(resp) => resp,
                Err(err) => {
                    if Self::advance_proxy_candidate(
                        &candidate_index,
                        candidates.as_slice(),
                        used_index,
                        "连接失败",
                    ) {
                        failures = 0;
                        continue;
                    }
                    failures += 1;
                    if failures >= 8 {
                        return Err(err.into());
                    }
                    sleep(Duration::from_millis(1500)).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 416 {
                if let Some(total) = total {
                    if cur >= total {
                        break;
                    }
                }
                return Err(anyhow!("HTTP 416"));
            }
            let bad_status = status != 206 && status != 200;
            let bad_length = status == 200 && Self::response_length_mismatch(&resp, total);
            if bad_status || bad_length {
                if Self::advance_proxy_candidate(
                    &candidate_index,
                    candidates.as_slice(),
                    used_index,
                    "响应异常",
                ) {
                    failures = 0;
                    continue;
                }
                if bad_status {
                    failures += 1;
                    if failures >= 8 {
                        return Err(anyhow!("HTTP {}", status));
                    }
                    sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                return Err(anyhow!("响应长度与预期不一致"));
            }
            failures = 0;

            let mut offset = cur;
            if status == 200 && cur > 0 {
                AsyncFile::create(&part_path).await?.set_len(0).await?;
                offset = 0;
                let mut state = entry.state.lock().await;
                state.downloaded = 0;
                if let Some(segment) = state.segments.get_mut(0) {
                    segment.cur = 0;
                    segment.done = false;
                }
                state.refresh_progress();
            }

            let file = AsyncOpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&part_path)
                .await?;
            let mut writer = BufferedWriter::new(file, offset).await?;
            let limit_exclusive = total
                .map(|value| value.saturating_add(1))
                .unwrap_or(u64::MAX);
            let mut stream = resp.bytes_stream();
            let mut stream_error = None;
            let mut cancelled = false;
            loop {
                let next = tokio::select! {
                    _ = token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    item = stream.next() => item,
                };
                let Some(chunk) = next else {
                    break;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        stream_error = Some(err);
                        break;
                    }
                };
                if chunk.is_empty() {
                    continue;
                }
                let take = if let Some(total) = total {
                    total
                        .saturating_sub(writer.offset())
                        .min(chunk.len() as u64) as usize
                } else {
                    chunk.len()
                };
                if take == 0 {
                    break;
                }
                if !entry.limiter.acquire(take, &token).await {
                    cancelled = true;
                    break;
                }
                writer.pending.extend_from_slice(&chunk[..take]);
                if writer.should_flush() {
                    offset = writer.flush_limit(limit_exclusive).await?;
                    let mut state = entry.state.lock().await;
                    state.downloaded = offset;
                    if let Some(segment) = state.segments.get_mut(0) {
                        segment.cur = offset;
                        if let Some(total) = total {
                            if offset >= total {
                                segment.done = true;
                            }
                        }
                    }
                    state.refresh_progress();
                }
                if let Some(total) = total {
                    if writer.offset() >= total {
                        break;
                    }
                }
            }

            offset = writer.flush_limit(limit_exclusive).await?;
            {
                let mut state = entry.state.lock().await;
                state.downloaded = offset;
                if let Some(segment) = state.segments.get_mut(0) {
                    segment.cur = offset;
                    if let Some(total) = total {
                        if offset >= total {
                            segment.done = true;
                        }
                    }
                }
                state.refresh_progress();
            }

            if cancelled {
                return Ok(());
            }
            if let Some(err) = stream_error {
                if failures >= 1 {
                    if Self::advance_proxy_candidate(
                        &candidate_index,
                        candidates.as_slice(),
                        used_index,
                        "下载流中断",
                    ) {
                        failures = 0;
                        continue;
                    }
                }
                failures += 1;
                if failures >= 8 {
                    return Err(err.into());
                }
                sleep(Duration::from_millis(1500)).await;
                continue;
            }
            if let Some(total) = total {
                if offset >= total {
                    break;
                }
                if failures >= 1 {
                    if Self::advance_proxy_candidate(
                        &candidate_index,
                        candidates.as_slice(),
                        used_index,
                        "响应提前结束",
                    ) {
                        failures = 0;
                        continue;
                    }
                }
                failures += 1;
                if failures >= 8 {
                    return Err(anyhow!("单流下载多次重试仍未完成"));
                }
                sleep(Duration::from_millis(1500)).await;
                continue;
            }
            break;
        }
        let mut state = entry.state.lock().await;
        if state.total.is_none() {
            if let Ok(meta) = fs::metadata(&state.part_path) {
                state.total = Some(meta.len());
                state.downloaded = meta.len();
                state.progress = 1.0;
                if let Some(segment) = state.segments.get_mut(0) {
                    segment.cur = meta.len();
                    segment.done = true;
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager(dir: &Path, patch: Value) -> Arc<TaskManager> {
        let settings = Arc::new(SettingsStore::load(dir).unwrap());
        settings.update_patch(&patch).unwrap();
        TaskManager::new(Some(dir.to_path_buf()), settings).unwrap()
    }

    #[test]
    fn proxy_candidate_advances_once_per_failed_index() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let index = AtomicUsize::new(0);
        assert!(TaskManager::advance_proxy_candidate(
            &index,
            &candidates,
            0,
            "test"
        ));
        assert_eq!(index.load(Ordering::SeqCst), 1);
        // 另一个 worker 仍拿着旧索引时，不应重复推进。
        assert!(TaskManager::advance_proxy_candidate(
            &index,
            &candidates,
            0,
            "test"
        ));
        assert_eq!(index.load(Ordering::SeqCst), 1);
        assert!(TaskManager::advance_proxy_candidate(
            &index,
            &candidates,
            1,
            "test"
        ));
        assert_eq!(index.load(Ordering::SeqCst), 2);
        assert!(!TaskManager::advance_proxy_candidate(
            &index,
            &candidates,
            2,
            "test"
        ));
    }

    #[tokio::test]
    async fn signed_urls_hit_remembered_duplicate_action() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({
                "defaultDir": dir.path().to_string_lossy(),
                "duplicateAction": 1,
                "rememberDuplicateAction": true
            }),
        );
        let first = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/file?a=1&sig=AAA&jwt=BBB",
                "filename": "x.bin",
                "start": false
            }))
            .await
            .unwrap();
        assert!(matches!(first, AddTaskResult::Added(_)));

        let second = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/file?a=1&sig=CCC&jwt=DDD",
                "filename": "x.bin",
                "start": false
            }))
            .await
            .unwrap();
        match second {
            AddTaskResult::Added(snap) => assert_eq!(snap.filename, "x (1).bin"),
            other => panic!("expected added duplicate copy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_action_is_not_remembered_as_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({
                "defaultDir": dir.path().to_string_lossy(),
                "duplicateAction": 0,
                "rememberDuplicateAction": true
            }),
        );
        m.add_task_checked(&serde_json::json!({
            "url": "https://example.com/file",
            "filename": "x.bin",
            "start": false
        }))
        .await
        .unwrap();
        let second = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/file",
                "filename": "x.bin",
                "start": false
            }))
            .await
            .unwrap();
        assert!(matches!(second, AddTaskResult::NeedsChoice { .. }));
    }

    #[tokio::test]
    async fn remembered_name_conflict_only_renames_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({
                "defaultDir": dir.path().to_string_lossy(),
                "fileConflictAction": 1,
                "rememberFileConflictAction": true
            }),
        );
        m.add_task_checked(&serde_json::json!({
            "url": "https://example.com/a",
            "filename": "same.bin",
            "start": false
        }))
        .await
        .unwrap();
        let conflict = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/b",
                "filename": "same.bin",
                "start": false
            }))
            .await
            .unwrap();
        match conflict {
            AddTaskResult::Added(snap) => assert_eq!(snap.filename, "same (1).bin"),
            other => panic!("expected renamed on conflict, got {other:?}"),
        }
        let no_conflict = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/c",
                "filename": "other.bin",
                "start": false
            }))
            .await
            .unwrap();
        match no_conflict {
            AddTaskResult::Added(snap) => assert_eq!(snap.filename, "other.bin"),
            other => panic!("expected unchanged filename, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_task_cleans_http_temp_files_but_keeps_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        let added = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/file.bin",
                "filename": "file.bin",
                "start": false
            }))
            .await
            .unwrap();
        let id = match added {
            AddTaskResult::Added(snap) => snap.id,
            other => panic!("expected added task, got {other:?}"),
        };

        let final_path = dir.path().join("file.bin");
        let part_path = dir.path().join("file.bin.fdmpart");
        let cfg_path = dir.path().join("file.bin.fdmcfg");
        std::fs::write(&final_path, b"completed payload").unwrap();
        std::fs::write(&part_path, b"partial payload").unwrap();
        std::fs::write(&cfg_path, b"{}").unwrap();

        assert!(m.remove_task(id, false).await);
        assert!(final_path.exists(), "保留文件模式不应删除最终文件");
        assert!(!part_path.exists(), "移除任务应删除 .fdmpart");
        assert!(!cfg_path.exists(), "移除任务应删除 .fdmcfg");
    }

    #[tokio::test]
    async fn remove_task_keeps_temp_files_shared_with_duplicate_overwrite_task() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        let first = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/shared.bin",
                "filename": "shared.bin",
                "start": false
            }))
            .await
            .unwrap();
        let first_id = match first {
            AddTaskResult::Added(snap) => snap.id,
            other => panic!("expected added task, got {other:?}"),
        };
        let second = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/shared.bin",
                "filename": "shared.bin",
                "duplicateAction": 2,
                "start": false
            }))
            .await
            .unwrap();
        let second_id = match second {
            AddTaskResult::Added(snap) => snap.id,
            other => panic!("expected overwrite duplicate task, got {other:?}"),
        };

        let part_path = dir.path().join("shared.bin.fdmpart");
        let cfg_path = dir.path().join("shared.bin.fdmcfg");
        std::fs::write(&part_path, b"partial payload").unwrap();
        std::fs::write(&cfg_path, b"{}").unwrap();

        assert!(m.remove_task(first_id, false).await);
        assert!(
            part_path.exists(),
            "共享临时文件仍被第二个任务引用，不能删除"
        );
        assert!(
            cfg_path.exists(),
            "共享断点配置仍被第二个任务引用，不能删除"
        );

        assert!(m.remove_task(second_id, false).await);
        assert!(!part_path.exists(), "最后一个引用移除后应删除 .fdmpart");
        assert!(!cfg_path.exists(), "最后一个引用移除后应删除 .fdmcfg");
    }

    #[tokio::test]
    async fn remove_task_with_delete_file_removes_final_and_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        let added = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/remove-all.bin",
                "filename": "remove-all.bin",
                "start": false
            }))
            .await
            .unwrap();
        let id = match added {
            AddTaskResult::Added(snap) => snap.id,
            other => panic!("expected added task, got {other:?}"),
        };

        let final_path = dir.path().join("remove-all.bin");
        let part_path = dir.path().join("remove-all.bin.fdmpart");
        let cfg_path = dir.path().join("remove-all.bin.fdmcfg");
        std::fs::write(&final_path, b"payload").unwrap();
        std::fs::write(&part_path, b"partial").unwrap();
        std::fs::write(&cfg_path, b"{}").unwrap();

        assert!(m.remove_task(id, true).await);
        assert!(!final_path.exists(), "删除文件模式应删除最终文件");
        assert!(!part_path.exists(), "删除文件模式应删除 .fdmpart");
        assert!(!cfg_path.exists(), "删除文件模式应删除 .fdmcfg");
    }

    #[tokio::test]
    async fn remove_all_with_delete_file_removes_final_files() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        for name in ["one.bin", "two.bin"] {
            m.add_task_checked(&serde_json::json!({
                "url": format!("https://example.com/{name}"),
                "filename": name,
                "start": false
            }))
            .await
            .unwrap();
        }
        let one_file = dir.path().join("one.bin");
        let two_file = dir.path().join("two.bin");
        std::fs::write(&one_file, b"one").unwrap();
        std::fs::write(&two_file, b"two").unwrap();

        assert!(m.remove_all(true).await);
        assert!(m.list_tasks().await.is_empty());
        assert!(!one_file.exists(), "删除全部并删除文件时应删除最终文件");
        assert!(!two_file.exists(), "删除全部并删除文件时应删除最终文件");
    }

    #[tokio::test]
    async fn remove_all_cleans_temp_files_and_persists_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        for name in ["one.bin", "two.bin"] {
            m.add_task_checked(&serde_json::json!({
                "url": format!("https://example.com/{name}"),
                "filename": name,
                "start": false
            }))
            .await
            .unwrap();
        }
        let one_part = dir.path().join("one.bin.fdmpart");
        let one_cfg = dir.path().join("one.bin.fdmcfg");
        let two_part = dir.path().join("two.bin.fdmpart");
        let two_cfg = dir.path().join("two.bin.fdmcfg");
        for path in [&one_part, &one_cfg, &two_part, &two_cfg] {
            std::fs::write(path, b"payload").unwrap();
        }

        assert!(m.remove_all(false).await);
        assert!(m.list_tasks().await.is_empty());
        assert!(!one_part.exists());
        assert!(!one_cfg.exists());
        assert!(!two_part.exists());
        assert!(!two_cfg.exists());

        let saved: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.path().join("tasks.json")).unwrap()).unwrap();
        assert!(saved.is_empty(), "remove_all 完成后 tasks.json 应为空");
    }

    #[tokio::test]
    async fn same_part_path_claims_are_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        let base = serde_json::json!({
            "url": "https://example.com/shared.bin",
            "filename": "shared.bin",
            "start": false
        });
        let first = match m.add_task_checked(&base).await.unwrap() {
            AddTaskResult::Added(snap) => snap,
            other => panic!("expected first task, got {other:?}"),
        };
        let mut overwrite = base.clone();
        overwrite["duplicateAction"] = serde_json::json!(2);
        let second = match m.add_task_checked(&overwrite).await.unwrap() {
            AddTaskResult::Added(snap) => snap,
            other => panic!("expected overwrite task, got {other:?}"),
        };
        assert_eq!(first.filepath, second.filepath);

        let key = normalize_path(&format!("{}.fdmpart", first.filepath));
        let claim = m.try_claim_part(&key).expect("first claim should succeed");
        assert!(
            m.try_claim_part(&key).is_none(),
            "第二个任务不得同时拿到同一 .fdmpart 的写入权"
        );
        drop(claim);
        assert!(
            m.try_claim_part(&key).is_some(),
            "前一个写入者释放后应可再次占用"
        );
    }

    #[tokio::test]
    async fn rejects_relative_download_dir() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(
            dir.path(),
            serde_json::json!({ "defaultDir": dir.path().to_string_lossy() }),
        );
        let result = m
            .add_task_checked(&serde_json::json!({
                "url": "https://example.com/relative.bin",
                "dir": "relative-dir",
                "filename": "relative.bin",
                "start": false
            }))
            .await;
        assert!(result.is_err(), "相对下载目录必须被拒绝");
    }
}
