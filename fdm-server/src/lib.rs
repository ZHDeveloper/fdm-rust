use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fdm_core::paths::{is_safe_open_path, is_torrent_path};
use fdm_core::{github_proxy_candidates, has_sensitive_headers, SettingsStore, TaskSnapshot};
use fdm_engine::{AddTaskResult, TaskManager};
use fdm_platform::PlatformService;
use futures_util::StreamExt;
use rand::RngCore;

pub mod proxy;

const BLOB_CHUNK_SIZE: usize = 4 * 1024 * 1024;
const BLOB_TICKET_TTL: Duration = Duration::from_secs(30);
const BLOB_CAPTURE_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const BLOB_TICKET_LIMIT: usize = 128;

#[derive(Clone)]
pub struct BlobTicket {
    filename: String,
    size: u64,
    page_url: String,
    created_at: Instant,
}

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

#[derive(RustEmbed)]
#[folder = "../app_ui"]
struct UiAssets;

#[derive(RustEmbed)]
#[folder = "../extension"]
struct ExtensionAssets;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub phase: String,
    pub current_version: String,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            phase: "idle".to_string(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            version: None,
            notes: None,
            downloaded: 0,
            total: None,
            error: None,
        }
    }
}

#[async_trait]
pub trait DesktopHooks: Send + Sync {
    async fn open_progress(&self, task_id: i64) -> Result<bool>;
    async fn open_settings(&self) -> Result<bool>;
    async fn open_extension_guide(&self) -> Result<bool>;
    async fn open_main(&self) -> Result<bool>;
    async fn open_new_download(&self, prefill: Value) -> Result<bool>;
    async fn open_duplicate_dialog(&self, id: &str, title: &str) -> Result<bool>;
    async fn close_progress(&self, task_id: i64) -> Result<bool>;
    async fn window_control(&self, action: &str, target: Option<String>, params: Value) -> Value;
    async fn app_version(&self) -> Result<String>;
    async fn update_status(&self) -> Result<UpdateStatus>;
    async fn check_update(&self) -> Result<Option<UpdateInfo>>;
    async fn install_update(&self, expected_version: &str) -> Result<()>;
    async fn quit(&self) -> Result<()>;
}

#[derive(Clone)]
pub struct ServerState {
    pub manager: Arc<TaskManager>,
    pub settings: Arc<SettingsStore>,
    pub platform: Arc<dyn PlatformService>,
    pub hooks: Arc<dyn DesktopHooks>,
    pub data_dir: PathBuf,
    /// 独立的重复下载询问窗口上下文：id -> {opts, duplicate, defaultAction, actions}
    pub duplicate_requests: Arc<tokio::sync::Mutex<HashMap<String, DuplicateRequest>>>,
    pub duplicate_seq: Arc<AtomicU64>,
    /// 浏览器扩展最近一次访问状态接口的时间，用于判断扩展是否已连接
    pub last_browser_seen: Arc<Mutex<Option<Instant>>>,
    /// 一次性 Blob 上传票据，键为随机 Token
    pub blob_tickets: Arc<Mutex<HashMap<String, BlobTicket>>>,
}

/// 弹窗待处理请求。带创建时间：窗口被直接关闭时不会走 cancel，
/// 没有时间戳就无法回收，条目（含自定义请求头）会一直驻留内存。
#[derive(Clone)]
pub struct DuplicateRequest {
    pub created_at: Instant,
    pub context: Value,
}

const DUPLICATE_TTL: Duration = Duration::from_secs(600);
const DUPLICATE_LIMIT: usize = 64;

/// 回收过期请求，并在数量超限时淘汰最旧的一批。
fn prune_duplicate_requests(pending: &mut HashMap<String, DuplicateRequest>) {
    pending.retain(|_, entry| entry.created_at.elapsed() < DUPLICATE_TTL);
    let excess = pending.len().saturating_sub(DUPLICATE_LIMIT);
    if excess == 0 {
        return;
    }
    let mut oldest: Vec<(Instant, String)> = pending
        .iter()
        .map(|(id, entry)| (entry.created_at, id.clone()))
        .collect();
    oldest.sort_by_key(|(at, _)| *at);
    for (_, id) in oldest.into_iter().take(excess) {
        pending.remove(&id);
    }
}

pub fn ensure_extension_assets(data_dir: &Path) -> Result<PathBuf> {
    let target = data_dir.join("extension");
    for path in ExtensionAssets::iter() {
        let rel = path.as_ref();
        if rel.is_empty() {
            continue;
        }
        let dest = target.join(rel);
        let Some(file) = ExtensionAssets::get(rel) else {
            continue;
        };
        let data = file.data.as_ref();
        // 升级时同步覆盖旧副本，避免 data/extension 永远停留在旧版本扩展
        if let Ok(existing) = std::fs::read(&dest) {
            if existing == data {
                continue;
            }
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, data)?;
    }
    Ok(target)
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/api/tasks", get(get_tasks).post(create_task))
        .route("/api/tasks/remove_all", post(remove_all_tasks))
        .route("/api/tasks/{task_id}", get(get_task).delete(delete_task))
        .route("/api/tasks/{task_id}/pause", post(pause_task))
        .route("/api/tasks/{task_id}/resume", post(resume_task))
        .route("/api/settings", get(get_settings).post(update_settings))
        .route("/api/system/message_box", post(system_message_box))
        .route("/api/system/version", get(system_version))
        .route("/api/system/update/status", get(update_status))
        .route("/api/system/update/check", post(update_check))
        .route("/api/system/update/install", post(update_install))
        .route("/api/system/default_dir", get(get_default_dir))
        .route("/api/system/probe_size", post(probe_size))
        .route("/api/system/choose_torrent_file", post(choose_torrent_file))
        .route("/api/system/probe_torrent", post(probe_torrent))
        .route("/api/engines/status", get(engines_status))
        .route("/api/engines/ed2k/reconnect", post(ed2k_reconnect))
        .route("/api/system/choose_dir", post(choose_dir))
        .route("/api/system/choose_save_as", post(choose_save_as))
        .route("/api/system/open_path", post(open_path))
        .route("/api/system/clipboard_read", post(clipboard_read))
        .route("/api/system/window", post(window_control))
        .route("/api/system/open_progress", post(open_progress))
        .route("/api/system/open_settings", post(open_settings))
        .route("/api/system/extension", get(extension_info))
        .route(
            "/api/system/open_extension_guide",
            post(open_extension_guide),
        )
        .route(
            "/api/system/open_browser_extensions",
            post(open_browser_extensions),
        )
        .route("/api/system/open_main", post(open_main))
        .route("/api/system/open_new_download", post(open_new_download))
        .route("/api/duplicate/open", post(duplicate_open))
        .route("/api/duplicate/context/{id}", get(duplicate_context))
        .route("/api/duplicate/submit/{id}", post(duplicate_submit))
        .route("/api/duplicate/cancel/{id}", post(duplicate_cancel))
        .route("/api/system/quit", post(app_quit))
        .route("/api/events", get(sse_events))
        .route("/api/browser/status", get(browser_status))
        .route("/api/browser/intercept", post(browser_intercept))
        .route("/api/browser/blob/prepare", post(blob_prepare))
        .route("/api/browser/blob/ws", get(blob_ws))
        .fallback(static_handler)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin, _| {
                    origin.to_str().map(is_allowed_origin).unwrap_or(false)
                }))
                .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
                .allow_headers(Any)
                .max_age(Duration::from_secs(600)),
        )
        // 放在 CORS 外层：先拒绝非本机 Host 与非白名单 Origin，再进入业务路由。
        .layer(middleware::from_fn(guard_local_request))
        .with_state(state)
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = host.rsplit_once(':').map(|(head, _)| head).unwrap_or(host);
    name.eq_ignore_ascii_case("127.0.0.1")
        || name.eq_ignore_ascii_case("localhost")
        || name == "[::1]"
}

fn is_allowed_origin(origin: &str) -> bool {
    origin == "http://127.0.0.1:25432"
        || origin == "http://localhost:25432"
        || origin.starts_with("chrome-extension://")
        || origin.starts_with("moz-extension://")
}

/// 本机 UI（Tauri 窗口加载的页面）的 origin。
fn is_local_ui_origin(origin: &str) -> bool {
    origin == "http://127.0.0.1:25432" || origin == "http://localhost:25432"
}

/// 高风险端点：只有本机 UI 或无 Origin 的原生客户端可以调用。
///
/// 浏览器扩展需要创建下载任务，但用户可能装了任意第三方扩展，放行
/// `chrome-extension://` 等于把“执行任意路径 / 退出应用 / 安装更新”也交出去。
/// 这些端点对扩展一律拒绝（原生客户端不带 Origin，仍可正常调试）。
/// 页面同源自带脚本/样式：禁外部脚本、禁 object/base，仅放行内联 style 与 data: 图片。
/// 与 `tauri.conf.json` 的 `app.security.csp` 保持一致（扩展打开的窗口不走 Tauri 侧）。
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self' ws://127.0.0.1:25432 ws://localhost:25432; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

const PRIVILEGED_ROUTE_PREFIXES: &[&str] = &[
    "/api/system/open_path",
    "/api/system/quit",
    "/api/system/window",
    "/api/system/probe_torrent",
    "/api/update",
];

/// 只接受本机 Host 与受信 Origin（本机 UI / 浏览器扩展），阻断任意网页的 CSRF/DNS rebinding。
async fn guard_local_request(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().host().map(str::to_owned));
    if !host.as_deref().map(is_loopback_host).unwrap_or(false) {
        return err_response(StatusCode::FORBIDDEN, "forbidden host");
    }
    // WebSocket 无法携带扩展自定义头，且浏览器内容脚本会带页面 Origin。
    // 该路由自行校验一次性 Ticket，其他请求仍执行完整的 Origin/CSRF 检查。
    let blob_ws = req.uri().path() == "/api/browser/blob/ws";
    // 扩展可通过自定义头证明自己不是网页（网页发自定义头必然先走被拦截的预检）。
    let trusted_extension = req.headers().contains_key("x-fdm-extension");
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let path = req.uri().path().to_string();
    if !blob_ws
        && PRIVILEGED_ROUTE_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
        && !origin.map(is_local_ui_origin).unwrap_or(true)
    {
        return err_response(StatusCode::FORBIDDEN, "forbidden origin");
    }
    if !blob_ws {
        if let Some(origin) = origin {
            if !is_allowed_origin(origin) && !trusted_extension {
                return err_response(StatusCode::FORBIDDEN, "forbidden origin");
            }
        }
        // 浏览器发起的跨站请求必须带受信 Origin；覆盖 <a ping> 这类不带 Origin 的简单请求。
        if let Some(site) = req
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
        {
            if !matches!(site, "same-origin" | "none")
                && !trusted_extension
                && !origin.map(is_allowed_origin).unwrap_or(false)
            {
                return err_response(StatusCode::FORBIDDEN, "forbidden cross-site request");
            }
        }
    }

    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // 页面全部脚本/样式都是同源自带，只放行内联 style 与 data: 图片；
    // 扩展打开的窗口不走 Tauri 的 CSP，因此服务端也要下发同一策略。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

pub async fn serve(listener: TcpListener, state: ServerState) -> Result<()> {
    spawn_completion_worker(state.clone());
    let proxy_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = proxy::run(proxy_state).await {
            tracing::warn!(error = %err, "本地 HTTP 代理未启动");
        }
    });
    let app = router(state);
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn bind_default() -> Result<TcpListener> {
    Ok(TcpListener::bind("127.0.0.1:25432").await?)
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match UiAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut response = Response::new(axum::body::Body::from(file.data.into_owned()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref())
                    .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            response
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    let payload = json!({"error": message.clone(), "detail": message});
    (status, Json(payload)).into_response()
}

#[derive(Debug, Deserialize, Default)]
struct DeleteFileQuery {
    #[serde(default)]
    delete_file: bool,
}

async fn get_tasks(State(state): State<ServerState>) -> Response {
    Json(state.manager.list_tasks().await).into_response()
}

async fn create_task(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "URL 不能为空");
    }
    if let Some(dir) = body.get("dir").and_then(Value::as_str) {
        if !dir.trim().is_empty() && !Path::new(dir).is_absolute() {
            return err_response(StatusCode::BAD_REQUEST, "下载目录必须是绝对路径");
        }
    }
    add_result_response(&state, state.manager.add_task_checked(&body).await).await
}

async fn add_result_response(state: &ServerState, result: Result<AddTaskResult>) -> Response {
    match result {
        Ok(AddTaskResult::Added(snap)) => Json(json!({
            "ok": true, "action": "added", "id": snap.id, "task": snap
        }))
        .into_response(),
        Ok(AddTaskResult::Existing { task, action }) => {
            if action == 3 && task.status == "done" && Path::new(&task.filepath).exists() {
                apply_completion_actions(state, &task).await;
                return Json(json!({
                    "ok": true,
                    "action": "existing_done",
                    "id": task.id,
                    "task": task
                }))
                .into_response();
            }
            Json(json!({
                "ok": true,
                "action": if action == 0 { "skipped" } else { "resumed" },
                "id": task.id,
                "task": task
            }))
            .into_response()
        }
        Ok(AddTaskResult::NeedsChoice {
            existing,
            default_action,
        }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "code": "duplicate",
                "error": "duplicate",
                "duplicate": existing,
                "defaultAction": default_action,
                "actions": duplicate_actions()
            })),
        )
            .into_response(),
        Ok(AddTaskResult::NameConflict {
            filename,
            path,
            reason,
            suggested,
            existing_id,
        }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "code": "name_conflict",
                "error": "name_conflict",
                "filename": filename,
                "path": path,
                "reason": reason,
                "suggested": suggested,
                "existingId": existing_id
            })),
        )
            .into_response(),
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

fn duplicate_actions() -> Value {
    json!([
        {"value": 1, "label": "为重复的下载链接添加带序号的文件名"},
        {"value": 2, "label": "添加重复的下载链接，并覆盖已存在的文件"},
        {"value": 3, "label": "若原任务已完成，按完成设置处理；否则继续下载"}
    ])
}

fn name_conflict_actions(suggested: &str) -> Value {
    json!([
        {"value": 1, "label": format!("使用带序号的文件名：{}", suggested)},
        {"value": 2, "label": "覆盖同名文件"}
    ])
}

/// 打开独立的“重复的下载链接”窗口。调用方传入完整的新建任务参数，
/// 窗口选择后由 duplicate_submit 直接创建/恢复任务并打开进度窗口。
async fn open_prompt_window(state: &ServerState, body: Value) -> Response {
    let id = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        state.duplicate_seq.fetch_add(1, Ordering::SeqCst)
    );
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("duplicate")
        .to_string();
    let mut context = body.clone();
    if let Some(obj) = context.as_object_mut() {
        obj.insert("kind".to_string(), json!(kind));
        if !obj.contains_key("opts") {
            obj.insert("opts".to_string(), json!({}));
        }
        if !obj.contains_key("duplicate") {
            obj.insert("duplicate".to_string(), json!({}));
        }
        let default_action = if kind == "name_conflict" { 1 } else { 3 };
        obj.entry("defaultAction".to_string())
            .or_insert_with(|| json!(default_action));
        if !obj.contains_key("actions") {
            let actions = if kind == "name_conflict" {
                let suggested = obj
                    .get("suggested")
                    .and_then(Value::as_str)
                    .unwrap_or("自动生成")
                    .to_string();
                name_conflict_actions(&suggested)
            } else {
                duplicate_actions()
            };
            obj.insert("actions".to_string(), actions);
        }
    }
    let title = if kind == "name_conflict" {
        "文件名已存在"
    } else {
        "重复的下载链接"
    };
    {
        let mut pending = state.duplicate_requests.lock().await;
        prune_duplicate_requests(&mut pending);
        pending.insert(
            id.clone(),
            DuplicateRequest {
                created_at: Instant::now(),
                context,
            },
        );
    }
    match state.hooks.open_duplicate_dialog(&id, title).await {
        Ok(true) => Json(json!({"ok": true, "id": id, "opened": true, "action": "prefill_dialog"})).into_response(),
        Ok(false) => Json(json!({"ok": false, "id": id, "opened": false, "action": "prefill_dialog"})).into_response(),
        Err(err) => {
            Json(json!({"ok": false, "id": id, "opened": false, "action": "prefill_dialog", "detail": err.to_string()}))
                .into_response()
        }
    }
}

async fn duplicate_open(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    open_prompt_window(&state, body).await
}

async fn duplicate_context(
    State(state): State<ServerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let context = state
        .duplicate_requests
        .lock()
        .await
        .get(&id)
        .map(|entry| entry.context.clone());
    match context {
        Some(context) => Json(context).into_response(),
        None => err_response(StatusCode::NOT_FOUND, "重复下载请求不存在或已处理"),
    }
}

async fn duplicate_submit(
    State(state): State<ServerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(entry) = state.duplicate_requests.lock().await.remove(&id) else {
        return err_response(StatusCode::NOT_FOUND, "重复下载请求不存在或已处理");
    };
    let context = entry.context;
    let mut opts = context.get("opts").cloned().unwrap_or(json!({}));
    let kind = context
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("duplicate");
    let action = body.get("action").and_then(Value::as_u64).unwrap_or(3) as u8;
    let remember = body
        .get("remember")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(obj) = opts.as_object_mut() {
        if kind == "name_conflict" {
            obj.insert("fileAction".to_string(), json!(action));
            obj.insert("rememberFileConflict".to_string(), json!(remember));
        } else {
            obj.insert("duplicateAction".to_string(), json!(action));
            obj.insert("rememberDuplicate".to_string(), json!(remember));
        }
    }
    add_result_response(&state, state.manager.add_task_checked(&opts).await).await
}

async fn duplicate_cancel(
    State(state): State<ServerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    state.duplicate_requests.lock().await.remove(&id);
    Json(json!({"ok": true})).into_response()
}

async fn get_task(
    State(state): State<ServerState>,
    axum::extract::Path(task_id): axum::extract::Path<i64>,
) -> Response {
    match state.manager.get_task(task_id).await {
        Some(snap) => Json(snap).into_response(),
        None => err_response(StatusCode::NOT_FOUND, "任务不存在"),
    }
}

async fn pause_task(
    State(state): State<ServerState>,
    axum::extract::Path(task_id): axum::extract::Path<i64>,
) -> Response {
    Json(json!({"ok": state.manager.pause_task(task_id).await})).into_response()
}

async fn resume_task(
    State(state): State<ServerState>,
    axum::extract::Path(task_id): axum::extract::Path<i64>,
) -> Response {
    Json(json!({"ok": state.manager.resume_task(task_id).await})).into_response()
}

async fn delete_task(
    State(state): State<ServerState>,
    axum::extract::Path(task_id): axum::extract::Path<i64>,
    Query(q): Query<DeleteFileQuery>,
) -> Response {
    let ok = state.manager.remove_task(task_id, q.delete_file).await;
    if ok {
        let _ = state.hooks.close_progress(task_id).await;
    }
    Json(json!({"ok": ok})).into_response()
}

async fn remove_all_tasks(
    State(state): State<ServerState>,
    Query(q): Query<DeleteFileQuery>,
) -> Response {
    let ids: Vec<i64> = state
        .manager
        .list_tasks()
        .await
        .into_iter()
        .map(|t| t.id)
        .collect();
    let ok = state.manager.remove_all(q.delete_file).await;
    if ok {
        for id in ids {
            let _ = state.hooks.close_progress(id).await;
        }
    }
    Json(json!({"ok": ok})).into_response()
}

async fn get_settings(State(state): State<ServerState>) -> Response {
    let mut value = state.settings.get_value();
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "githubProxyPresets".to_string(),
            json!(fdm_core::GITHUB_PROXY_PRESETS),
        );
    }
    Json(value).into_response()
}

async fn update_settings(State(state): State<ServerState>, Json(patch): Json<Value>) -> Response {
    if let Some(dir) = patch.get("defaultDir").and_then(Value::as_str) {
        if dir.trim().is_empty() || !Path::new(dir).is_absolute() {
            return err_response(StatusCode::BAD_REQUEST, "下载目录必须是绝对路径");
        }
    }
    match state.settings.update_patch(&patch) {
        Ok(updated) => {
            if patch.get("autoLaunch").is_some() {
                let platform = state.platform.clone();
                let enabled = updated.auto_launch;
                let _ =
                    tokio::task::spawn_blocking(move || platform.sync_auto_launch(enabled)).await;
            }
            if patch.get("fileAssociations").is_some() {
                let platform = state.platform.clone();
                let enabled = updated.file_associations;
                let _ =
                    tokio::task::spawn_blocking(move || platform.sync_file_associations(enabled))
                        .await;
            }
            if patch.get("defaultDir").is_some() {
                state.manager.apply_default_dir(&updated.default_dir).await;
            }
            let value = serde_json::to_value(&updated).unwrap_or(Value::Null);
            state.manager.broadcast("settings:changed", value.clone());
            Json(value).into_response()
        }
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn system_version(State(state): State<ServerState>) -> Response {
    match state.hooks.app_version().await {
        Ok(version) => Json(json!({"version": version})).into_response(),
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn update_status(State(state): State<ServerState>) -> Response {
    match state.hooks.update_status().await {
        Ok(status) => Json(status).into_response(),
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn update_check(State(state): State<ServerState>) -> Response {
    match state.hooks.check_update().await {
        Ok(Some(info)) => Json(json!({"available": true, "update": info})).into_response(),
        Ok(None) => Json(json!({"available": false})).into_response(),
        Err(err) => err_response(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn update_install(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let version = body
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if version.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "缺少更新版本号");
    }
    match state.hooks.install_update(&version).await {
        Ok(()) => Json(json!({"started": true, "version": version})).into_response(),
        Err(err) => err_response(StatusCode::CONFLICT, err.to_string()),
    }
}

async fn system_message_box(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("FDM")
        .to_string();
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_lowercase();
    let confirm = body
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let platform = state.platform.clone();
    match tokio::task::spawn_blocking(move || {
        platform.message_box(&title, &message, &kind, confirm)
    })
    .await
    {
        Ok(confirmed) => {
            Json(json!({"result": if confirmed { 1 } else { 2 }, "confirmed": confirmed}))
                .into_response()
        }
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn get_default_dir(State(state): State<ServerState>) -> Response {
    Json(json!({"path": state.settings.get().default_dir})).into_response()
}
async fn probe_size(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Json(json!({"ok": false, "size": null})).into_response();
    }
    // 同 URL 的历史任务已知大小：直接返回，避免 chunked 响应探测不到
    if let Some(task) = state
        .manager
        .list_tasks()
        .await
        .into_iter()
        .find(|t| t.url == url && t.total.unwrap_or(0) > 0)
    {
        return Json(json!({
            "ok": true,
            "size": task.total,
            "filename": task.filename,
            "source": "existing"
        }))
        .into_response();
    }
    let headers: std::collections::HashMap<String, String> = body
        .get("headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let client = match state.manager.http_client() {
        Ok(client) => client,
        Err(_) => return Json(json!({"ok": false, "size": null})).into_response(),
    };
    let settings = state.settings.get();
    let candidates = github_proxy_candidates(&url, &settings, has_sensitive_headers(&headers));
    match fdm_engine::probe::probe_candidates(&client, &candidates, &headers, "").await {
        Ok((result, _)) => {
            Json(json!({"ok": true, "size": result.total, "filename": result.filename}))
                .into_response()
        }
        Err(err) => {
            Json(json!({"ok": false, "size": null, "error": err.to_string()})).into_response()
        }
    }
}

async fn choose_torrent_file(State(state): State<ServerState>) -> Response {
    let platform = state.platform.clone();
    match tokio::task::spawn_blocking(move || {
        platform.choose_file("选择种子文件", "Torrent 种子", &["torrent"])
    })
    .await
    {
        Ok(Some(path)) => Json(json!({"path": path.to_string_lossy()})).into_response(),
        _ => Json(json!({"path": null})).into_response(),
    }
}

async fn probe_torrent(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let Some(bt) = state.manager.bt_engine() else {
        return Json(json!({"ok": false, "error": "BT 引擎未启用"})).into_response();
    };
    let bytes = if let Some(path) = body.get("path").and_then(Value::as_str) {
        // 种子读取只接受本地 .torrent：该接口会把任意文件内容读进内存并复制进数据目录。
        if !is_torrent_path(Path::new(path)) {
            return Json(json!({"ok": false, "error": "只支持本地 .torrent 文件"})).into_response();
        }
        match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                return Json(json!({"ok": false, "error": err.to_string()})).into_response()
            }
        }
    } else if let Some(url) = body.get("url").and_then(Value::as_str) {
        let client = match state.manager.http_client() {
            Ok(client) => client,
            Err(err) => {
                return Json(json!({"ok": false, "error": err.to_string()})).into_response()
            }
        };
        let settings = state.settings.get();
        let candidates = github_proxy_candidates(url, &settings, false);
        match fdm_engine::probe::fetch_torrent_bytes_candidates(client.as_ref(), &candidates).await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                return Json(json!({"ok": false, "error": err.to_string()})).into_response()
            }
        }
    } else {
        return Json(json!({"ok": false, "error": "缺少 path 或 url"})).into_response();
    };
    match bt.inspect_torrent(bytes).await {
        Ok(info) => Json(json!({
            "ok": true,
            "name": info.name,
            "total": info.total,
            "infoHash": info.info_hash,
            "files": info.files,
        }))
        .into_response(),
        Err(err) => Json(json!({"ok": false, "error": err.to_string()})).into_response(),
    }
}

async fn engines_status(State(state): State<ServerState>) -> Response {
    let bt_engine = state.manager.bt_engine();
    let bt = match bt_engine.as_ref() {
        Some(engine) => json!({
            "ready": true,
            "dhtNodes": engine.dht_nodes(),
            "listenPort": engine.listen_port(),
        }),
        None => json!({"ready": false}),
    };
    let ed2k_engine = state.manager.ed2k_engine();
    let ed2k = match ed2k_engine.as_ref() {
        Some(engine) => {
            let status = engine.status().await;
            let download_dir = engine.download_dir().to_string_lossy().into_owned();
            json!({
                "ready": status.ready,
                "serverConnected": status.server_connected,
                "kadConnected": status.kad_connected,
                "lowId": status.low_id,
                "serverName": status.server_name,
                "downloadDir": download_dir,
            })
        }
        None => json!({"ready": false}),
    };
    Json(json!({"bt": bt, "ed2k": ed2k})).into_response()
}

async fn ed2k_reconnect(State(state): State<ServerState>) -> Response {
    if let Some(engine) = state.manager.ed2k_engine() {
        if engine.reconnect().await {
            return Json(json!({"ok": true})).into_response();
        }
    }
    // 引擎尚未创建或已经失活时，这里必须真正重建，而不是只返回错误。
    match state.manager.restart_ed2k_engine().await {
        Ok(Some(engine)) => {
            let status = engine.status().await;
            Json(json!({
                "ok": status.ready,
                "restarted": true,
                "ready": status.ready,
            }))
            .into_response()
        }
        Ok(None) => Json(json!({"ok": false, "error": "ED2K 未配置或已禁用"})).into_response(),
        Err(err) => Json(json!({"ok": false, "error": err.to_string()})).into_response(),
    }
}

async fn choose_dir(State(state): State<ServerState>) -> Response {
    let platform = state.platform.clone();
    let initial = PathBuf::from(state.settings.get().default_dir);
    match tokio::task::spawn_blocking(move || platform.choose_dir("选择下载目录", &initial)).await
    {
        Ok(path) => {
            Json(json!({"path": path.map(|p| p.to_string_lossy().into_owned())})).into_response()
        }
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn choose_save_as(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let platform = state.platform.clone();
    let initial = PathBuf::from(state.settings.get().default_dir);
    match tokio::task::spawn_blocking(move || platform.choose_save_as("另存为", &initial, &name))
        .await
    {
        Ok(path) => {
            Json(json!({"path": path.map(|p| p.to_string_lossy().into_owned())})).into_response()
        }
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn open_path(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let raw = body
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 打开路径会被系统按文件类型执行：拒绝 UNC 远程路径与可执行/脚本类型，
    // 避免本地 API 变成“执行任意程序”的入口。
    if !is_safe_open_path(Path::new(&raw)) {
        tracing::warn!(path = %raw, "拒绝打开非本机或可执行路径");
        return Json(json!({"ok": false, "error": "不支持打开该路径"})).into_response();
    }
    let platform = state.platform.clone();
    let ok = tokio::task::spawn_blocking(move || platform.open_path(Path::new(&raw)))
        .await
        .unwrap_or(false);
    Json(json!({"ok": ok})).into_response()
}

async fn clipboard_read(State(state): State<ServerState>) -> Response {
    let platform = state.platform.clone();
    let text = tokio::task::spawn_blocking(move || platform.read_clipboard())
        .await
        .unwrap_or_default();
    Json(json!({"text": text})).into_response()
}

async fn window_control(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let target = body
        .get("target")
        .or_else(|| body.get("win_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Json(state.hooks.window_control(&action, target, body).await).into_response()
}

async fn open_progress(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    let task_id = body.get("taskId").and_then(Value::as_i64);
    if let Some(task_id) = task_id {
        let opened = state.hooks.open_progress(task_id).await.unwrap_or(false);
        Json(json!({"opened": opened})).into_response()
    } else {
        Json(json!({"opened": false})).into_response()
    }
}

async fn open_settings(State(state): State<ServerState>) -> Response {
    Json(json!({"opened": state.hooks.open_settings().await.unwrap_or(false)})).into_response()
}

async fn extension_info(State(state): State<ServerState>) -> Response {
    Json(json!({
        "dir": state.platform.extension_dir().to_string_lossy(),
        "name": "FDM 浏览器下载助手",
        "connected": browser_connected(&state),
    }))
    .into_response()
}

async fn open_extension_guide(State(state): State<ServerState>) -> Response {
    Json(json!({"opened": state.hooks.open_extension_guide().await.unwrap_or(false)}))
        .into_response()
}

async fn open_browser_extensions(
    State(state): State<ServerState>,
    Json(body): Json<Value>,
) -> Response {
    let browser = body
        .get("browser")
        .and_then(Value::as_str)
        .unwrap_or("chrome")
        .to_lowercase();
    let platform = state.platform.clone();
    let opened = tokio::task::spawn_blocking(move || platform.open_browser_extensions(&browser))
        .await
        .unwrap_or(false);
    Json(json!({"opened": opened})).into_response()
}

async fn open_main(State(state): State<ServerState>) -> Response {
    Json(json!({"opened": state.hooks.open_main().await.unwrap_or(false)})).into_response()
}

async fn open_new_download(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    Json(json!({"opened": state.hooks.open_new_download(body).await.unwrap_or(false)}))
        .into_response()
}

async fn app_quit(State(state): State<ServerState>) -> Response {
    let _ = state.hooks.quit().await;
    Json(json!({"ok": true})).into_response()
}

async fn sse_events(State(state): State<ServerState>) -> Response {
    let initial = tokio_stream::once(Ok::<SseEvent, Infallible>(
        SseEvent::default().comment("connected"),
    ));
    let receiver = state.manager.subscribe();
    let events = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok::<SseEvent, Infallible>(
                SseEvent::default()
                    .event(event.event)
                    .data(event.data.to_string()),
            )),
            Err(_) => None,
        }
    });
    Sse::new(initial.chain(events))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(12))
                .text("keepalive"),
        )
        .into_response()
}

fn mark_browser_seen(state: &ServerState) {
    if let Ok(mut seen) = state.last_browser_seen.lock() {
        *seen = Some(Instant::now());
    }
}

fn browser_connected(state: &ServerState) -> bool {
    state
        .last_browser_seen
        .lock()
        .ok()
        .and_then(|seen| *seen)
        // 扩展休眠时由 chrome.alarms（约 30 秒）唤醒，放宽到 75 秒避免状态闪烁。
        .map(|seen| seen.elapsed() < Duration::from_secs(75))
        .unwrap_or(false)
}

fn format_speed(speed_bytes: f64) -> String {
    if speed_bytes >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB/s", speed_bytes / (1024.0 * 1024.0 * 1024.0))
    } else if speed_bytes >= 1024.0 * 1024.0 {
        format!("{:.2} MB/s", speed_bytes / (1024.0 * 1024.0))
    } else if speed_bytes >= 1024.0 {
        format!("{:.1} KB/s", speed_bytes / 1024.0)
    } else {
        format!("{} B/s", speed_bytes as u64)
    }
}

/// 读取操作系统代理（Windows 注册表 / macOS scutil），带 5 秒缓存避免高频轮询开销。
fn detect_system_proxy() -> Option<String> {
    static CACHE: std::sync::Mutex<Option<(Instant, Option<String>)>> = std::sync::Mutex::new(None);
    if let Ok(mut guard) = CACHE.lock() {
        if let Some((at, val)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(5) {
                return val.clone();
            }
        }
        let fresh = detect_system_proxy_inner();
        *guard = Some((Instant::now(), fresh.clone()));
        fresh
    } else {
        detect_system_proxy_inner()
    }
}

#[cfg(windows)]
fn detect_system_proxy_inner() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    normalize_proxy_server(&server)
}

#[cfg(target_os = "macos")]
fn detect_system_proxy_inner() -> Option<String> {
    let output = std::process::Command::new("scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| -> Option<String> {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(key).map(|v| v.trim().to_string()))
    };
    if value("HTTPEnable:").as_deref() != Some("1") {
        return None;
    }
    Some(format!("{}:{}", value("HTTPProxy:")?, value("HTTPPort:")?))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn detect_system_proxy_inner() -> Option<String> {
    None
}

#[cfg(windows)]
fn normalize_proxy_server(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.contains('=') {
        return Some(raw.to_string());
    }
    let mut http = None;
    let mut any = None;
    for part in raw.split(';') {
        if let Some((key, value)) = part.split_once('=') {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            any.get_or_insert_with(|| value.to_string());
            if key.trim().eq_ignore_ascii_case("http") {
                http = Some(value.to_string());
            }
        }
    }
    http.or(any)
}

async fn browser_status(State(state): State<ServerState>) -> Response {
    mark_browser_seen(&state);
    let tasks = state.manager.list_tasks().await;
    let active: Vec<_> = tasks
        .iter()
        .filter(|t| t.status == "downloading" || t.status == "connecting")
        .collect();
    let total_speed: u64 = active.iter().map(|t| t.speed).sum();
    let settings = state.settings.get();
    let system_proxy = detect_system_proxy();
    Json(json!({
        "online": true, "name": "FDM", "version": env!("CARGO_PKG_VERSION"),
        "active_tasks": active.len(), "total_tasks": tasks.len(),
        "total_speed": total_speed, "total_speed_str": format_speed(total_speed as f64),
        "download_dir": settings.default_dir,
        "blob_capture": true,
        // 检测到系统代理时不要接管浏览器：PAC 无法表达 system 模式，会绕过用户代理。
        "proxy_ready": crate::proxy::is_ready() && system_proxy.is_none(),
        "system_proxy": system_proxy
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlobPrepareRequest {
    #[serde(default)]
    filename: String,
    size: u64,
    #[serde(default)]
    page_url: String,
}

#[derive(Debug, Deserialize)]
struct BlobTokenQuery {
    token: String,
}

fn new_blob_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn blob_prepare(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<BlobPrepareRequest>,
) -> Response {
    mark_browser_seen(&state);
    let filename = body.filename.trim();
    let filename = if filename.is_empty() {
        "download.bin"
    } else {
        filename
    };
    let token = new_blob_token();
    let now = Instant::now();
    {
        let Ok(mut tickets) = state.blob_tickets.lock() else {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Blob 会话锁损坏");
        };
        tickets.retain(|_, ticket| now.duration_since(ticket.created_at) < BLOB_TICKET_TTL);
        if tickets.len() >= BLOB_TICKET_LIMIT {
            return err_response(StatusCode::SERVICE_UNAVAILABLE, "Blob 会话过多");
        }
        tickets.insert(
            token.clone(),
            BlobTicket {
                filename: filename.to_string(),
                size: body.size,
                page_url: body.page_url.trim().to_string(),
                created_at: now,
            },
        );
    }
    // 已由 guard_local_request 校验为 loopback Host；沿用请求 Host 让测试与自定义端口一致。
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:25432");
    Json(json!({
        "ok": true,
        "wsUrl": format!("ws://{host}/api/browser/blob/ws?token={token}")
    }))
    .into_response()
}

async fn blob_ws(
    State(state): State<ServerState>,
    Query(query): Query<BlobTokenQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if query.token.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "Missing Blob token");
    }
    let ticket = {
        let Ok(mut tickets) = state.blob_tickets.lock() else {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Blob 会话锁损坏");
        };
        let now = Instant::now();
        tickets.retain(|_, ticket| now.duration_since(ticket.created_at) < BLOB_TICKET_TTL);
        tickets.remove(&query.token)
    };
    let Some(ticket) = ticket else {
        return err_response(StatusCode::FORBIDDEN, "Blob 会话无效或已过期");
    };

    ws.max_message_size(BLOB_CHUNK_SIZE + 1024)
        .max_frame_size(BLOB_CHUNK_SIZE + 1024)
        .on_upgrade(move |socket| async move {
            if let Err(err) = handle_blob_ws(state, socket, ticket).await {
                tracing::warn!(error = %err, "Blob 下载通道结束");
            }
        })
}

async fn send_ws_json(socket: &mut WebSocket, value: Value) -> Result<()> {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(Into::into)
}

async fn handle_blob_ws(
    state: ServerState,
    mut socket: WebSocket,
    ticket: BlobTicket,
) -> Result<()> {
    let page_url = if ticket.page_url.is_empty() {
        "blob://browser"
    } else {
        ticket.page_url.as_str()
    };
    let (id, part_path, capture_entry) = match state
        .manager
        .begin_capture(page_url, &ticket.filename, Some(ticket.size))
        .await
    {
        Ok(value) => value,
        Err(err) => {
            let _ = send_ws_json(
                &mut socket,
                json!({"type": "error", "message": err.to_string()}),
            )
            .await;
            return Err(err);
        }
    };
    if let Err(err) = state.hooks.open_progress(id).await {
        tracing::warn!(error = %err, "打开 Blob 下载进度窗口失败");
    }

    let outcome = run_blob_ws(&state, &mut socket, id, &part_path, ticket.size).await;
    let result = match outcome {
        Ok(written) => match state.manager.finish_capture(id, written).await {
            Ok(_) => {
                let _ = send_ws_json(&mut socket, json!({"type": "done", "taskId": id})).await;
                Ok(())
            }
            Err(err) => {
                if state.manager.capture_exists(id).await {
                    state.manager.fail_capture(id, &err.to_string()).await;
                }
                let _ = send_ws_json(
                    &mut socket,
                    json!({"type": "error", "message": err.to_string()}),
                )
                .await;
                Err(err)
            }
        },
        Err(err) => {
            if state.manager.capture_exists(id).await {
                state.manager.fail_capture(id, &err.to_string()).await;
                let _ = send_ws_json(
                    &mut socket,
                    json!({"type": "error", "message": err.to_string()}),
                )
                .await;
            }
            Err(err)
        }
    };
    capture_entry.mark_stopped();
    let _ = socket.send(Message::Close(None)).await;
    result
}

async fn run_blob_ws(
    state: &ServerState,
    socket: &mut WebSocket,
    id: i64,
    part_path: &Path,
    expected_size: u64,
) -> Result<u64> {
    let mut file = tokio::fs::File::create(part_path).await?;
    let mut received = 0_u64;
    let mut last_emit = Instant::now() - BLOB_CAPTURE_PROGRESS_INTERVAL;
    let mut last_sample = Instant::now();
    let mut last_bytes = 0_u64;
    send_ws_json(socket, json!({"type": "ready", "taskId": id})).await?;

    loop {
        if state.manager.capture_should_stop(id).await || !state.manager.capture_exists(id).await {
            return Err(anyhow::anyhow!("Blob 任务已取消"));
        }
        let message = match tokio::time::timeout(Duration::from_millis(500), socket.recv()).await {
            Err(_) => continue,
            Ok(None) => return Err(anyhow::anyhow!("浏览器连接已关闭")),
            Ok(Some(Err(err))) => return Err(err.into()),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Binary(data) => {
                if data.len() > BLOB_CHUNK_SIZE {
                    return Err(anyhow::anyhow!("Blob 分块超过 4 MiB"));
                }
                let next = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Blob 大小溢出"))?;
                if next > expected_size {
                    return Err(anyhow::anyhow!("Blob 数据超过声明大小"));
                }
                file.write_all(&data).await?;
                received = next;
                if last_emit.elapsed() >= BLOB_CAPTURE_PROGRESS_INTERVAL {
                    let dt = last_sample.elapsed().as_secs_f64().max(0.001);
                    let speed = (received - last_bytes) as f64 / dt;
                    let _ = state
                        .manager
                        .update_capture(id, received, Some(expected_size), speed)
                        .await;
                    last_bytes = received;
                    last_sample = Instant::now();
                    last_emit = Instant::now();
                }
                send_ws_json(socket, json!({"type": "ack", "received": received})).await?;
            }
            Message::Text(text) => {
                let value: Value = serde_json::from_str(text.as_str())?;
                match value.get("type").and_then(Value::as_str) {
                    Some("end") => {
                        if received != expected_size {
                            return Err(anyhow::anyhow!(
                                "Blob 数据不完整：收到 {received} / {expected_size} 字节"
                            ));
                        }
                        file.flush().await?;
                        let _ = state
                            .manager
                            .update_capture(id, received, Some(expected_size), 0.0)
                            .await;
                        return Ok(received);
                    }
                    Some("cancel") => return Err(anyhow::anyhow!("浏览器取消 Blob 下载")),
                    _ => return Err(anyhow::anyhow!("未知 Blob 控制消息")),
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            Message::Close(_) => return Err(anyhow::anyhow!("浏览器关闭 Blob 连接")),
        }
    }
}

async fn browser_intercept(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    mark_browser_seen(&state);
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "Missing download URL");
    }
    let filename = body
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let file_size = body
        .get("fileSize")
        .or_else(|| body.get("size"))
        .cloned()
        .unwrap_or(json!(0));
    let referrer = body
        .get("referrer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut headers = body
        .get("headers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let confirm = body.get("confirm").and_then(Value::as_bool).unwrap_or(true);
    let force_dialog = body
        .get("forceDialog")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let settings = state.settings.get();
    let auto_download = settings.auto_download;

    if confirm && (force_dialog || !auto_download) {
        let prefill = json!({"url": url, "filename": filename, "size": file_size,
            "referrer": referrer, "headers": headers});
        return match state.hooks.open_new_download(prefill).await {
            Ok(true) => Json(json!({"ok": true, "action": "prefill_dialog", "opened": true})).into_response(),
            Ok(false) => Json(json!({"ok": false, "action": "dialog_failed", "opened": false})).into_response(),
            Err(err) => Json(json!({"ok": false, "action": "dialog_error", "opened": false, "detail": err.to_string()})).into_response(),
        };
    }

    if !referrer.is_empty() && !headers.contains_key("Referer") && !headers.contains_key("referer")
    {
        headers.insert("Referer".to_string(), Value::String(referrer.clone()));
    }
    let mut opts = json!({"url": url, "filename": filename, "headers": Value::Object(headers)});
    let result = state.manager.add_task_checked(&opts).await;
    // 浏览器自动接管时不要先闪一下“下载文件信息”窗口：
    // 直接把重复/同名冲突作为独立询问窗口打开，用户选择后再创建任务并打开进度窗口。
    if auto_download {
        if let Ok(AddTaskResult::NeedsChoice {
            existing,
            default_action,
        }) = &result
        {
            let prompt = json!({
                "kind": "duplicate",
                "opts": opts.clone(),
                "duplicate": existing,
                "defaultAction": *default_action,
                "actions": duplicate_actions()
            });
            return open_prompt_window(&state, prompt).await;
        }
        if let Ok(AddTaskResult::NameConflict {
            filename,
            path,
            reason,
            suggested,
            existing_id,
        }) = &result
        {
            let prompt = json!({
                "kind": "name_conflict",
                "opts": opts.clone(),
                "filename": filename,
                "path": path,
                "reason": reason,
                "suggested": suggested,
                "existingId": existing_id,
                "defaultAction": 1,
                "actions": name_conflict_actions(suggested)
            });
            return open_prompt_window(&state, prompt).await;
        }
    }
    let result = if let Ok(AddTaskResult::NeedsChoice { default_action, .. }) = &result {
        if let Some(obj) = opts.as_object_mut() {
            obj.insert("duplicateAction".to_string(), json!(*default_action));
        }
        state.manager.add_task_checked(&opts).await
    } else if let Ok(AddTaskResult::NameConflict { .. }) = &result {
        if let Some(obj) = opts.as_object_mut() {
            obj.insert("fileAction".to_string(), json!(1));
        }
        state.manager.add_task_checked(&opts).await
    } else {
        result
    };
    match result {
        Ok(AddTaskResult::Added(snap)) => {
            let mut opened = false;
            if auto_download {
                opened = state.hooks.open_progress(snap.id).await.unwrap_or(false);
            }
            Json(json!({
                "ok": true,
                "action": "task_created",
                "outcome": "created",
                "task_id": snap.id,
                "opened": opened
            }))
            .into_response()
        }
        Ok(AddTaskResult::Existing { task, action }) => {
            if action == 3 && task.status == "done" && Path::new(&task.filepath).exists() {
                apply_completion_actions(&state, &task).await;
                return Json(json!({
                    // 统一返回 task_created，兼容旧版扩展：旧版只识别 task_created/
                    // prefill_dialog，否则会把已接管的下载重新交给浏览器。
                    "ok": true,
                    "action": "task_created",
                    "outcome": "existing_done",
                    "task_id": task.id,
                    "opened": false
                }))
                .into_response();
            }
            let mut opened = false;
            if auto_download && action != 0 {
                opened = state.hooks.open_progress(task.id).await.unwrap_or(false);
            }
            Json(json!({
                "ok": true,
                "action": "task_created",
                "outcome": if action == 0 { "skipped" } else { "existing" },
                "task_id": task.id,
                "opened": opened
            }))
            .into_response()
        }
        Ok(AddTaskResult::NeedsChoice { .. }) | Ok(AddTaskResult::NameConflict { .. }) => {
            err_response(StatusCode::CONFLICT, "duplicate or filename conflict")
        }
        Err(err) => err_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn apply_completion_actions(state: &ServerState, snap: &TaskSnapshot) {
    let settings = state.settings.get();
    let folder = PathBuf::from(&snap.dir);
    let file_path = if snap.filepath.is_empty() {
        folder.join(&snap.filename)
    } else {
        PathBuf::from(&snap.filepath)
    };
    if settings.done_open_file {
        let platform = state.platform.clone();
        let _ = tokio::task::spawn_blocking(move || platform.open_path(&file_path)).await;
    } else if settings.done_open_folder {
        let platform = state.platform.clone();
        let _ = tokio::task::spawn_blocking(move || platform.open_path(&folder)).await;
    }
    if settings.done_close_progress {
        let hooks = state.hooks.clone();
        let id = snap.id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            let _ = hooks.close_progress(id).await;
        });
    }
}

fn spawn_completion_worker(state: ServerState) {
    let mut receiver = state.manager.subscribe();
    tokio::spawn(async move {
        let mut handled: HashSet<i64> = HashSet::new();
        loop {
            let event = match receiver.recv().await {
                Ok(event) => event,
                // 发送端已释放：继续循环只会让这个任务 100% 占用一个 CPU 核。
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                // 消费不过来被跳过，立刻尝试接收下一条。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            };
            if event.event == "task:remove" {
                if let Some(id) = event.data.get("id").and_then(Value::as_i64) {
                    handled.remove(&id);
                    let hooks = state.hooks.clone();
                    tokio::spawn(async move {
                        let _ = hooks.close_progress(id).await;
                    });
                }
                continue;
            }
            if event.event != "task:update" {
                continue;
            }
            let Ok(snap) = serde_json::from_value::<TaskSnapshot>(event.data) else {
                continue;
            };
            if snap.status != "done" {
                handled.remove(&snap.id);
                continue;
            }
            if !handled.insert(snap.id) {
                continue;
            }
            apply_completion_actions(&state, &snap).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tower::ServiceExt;

    struct MockPlatform {
        dir: PathBuf,
        opened_browsers: Arc<Mutex<Vec<String>>>,
    }

    impl PlatformService for MockPlatform {
        fn default_download_dir(&self) -> PathBuf {
            self.dir.clone()
        }

        fn open_path(&self, _path: &Path) -> bool {
            true
        }

        fn reveal_path(&self, _path: &Path) -> bool {
            true
        }

        fn read_clipboard(&self) -> String {
            String::new()
        }

        fn choose_dir(&self, _title: &str, _initial: &Path) -> Option<PathBuf> {
            None
        }

        fn choose_save_as(
            &self,
            _title: &str,
            _initial: &Path,
            _default_name: &str,
        ) -> Option<PathBuf> {
            None
        }

        fn choose_file(
            &self,
            _title: &str,
            _filter_name: &str,
            _extensions: &[&str],
        ) -> Option<PathBuf> {
            None
        }

        fn message_box(&self, _title: &str, _message: &str, _kind: &str, _confirm: bool) -> bool {
            false
        }

        fn open_browser_extensions(&self, browser: &str) -> bool {
            self.opened_browsers
                .lock()
                .unwrap()
                .push(browser.to_string());
            true
        }

        fn sync_auto_launch(&self, _enabled: bool) -> bool {
            true
        }

        fn sync_file_associations(&self, _enabled: bool) -> bool {
            true
        }

        fn extension_dir(&self) -> PathBuf {
            self.dir.clone()
        }
    }

    struct MockHooks {
        guide_opened: AtomicBool,
        closed_progress: Arc<Mutex<Vec<i64>>>,
    }

    #[async_trait]
    impl DesktopHooks for MockHooks {
        async fn open_progress(&self, _task_id: i64) -> Result<bool> {
            Ok(true)
        }

        async fn open_settings(&self) -> Result<bool> {
            Ok(true)
        }

        async fn open_extension_guide(&self) -> Result<bool> {
            self.guide_opened.store(true, Ordering::SeqCst);
            Ok(true)
        }

        async fn open_main(&self) -> Result<bool> {
            Ok(true)
        }

        async fn open_new_download(&self, _prefill: Value) -> Result<bool> {
            Ok(true)
        }

        async fn open_duplicate_dialog(&self, _id: &str, _title: &str) -> Result<bool> {
            Ok(true)
        }

        async fn close_progress(&self, task_id: i64) -> Result<bool> {
            self.closed_progress.lock().unwrap().push(task_id);
            Ok(true)
        }

        async fn window_control(
            &self,
            _action: &str,
            _target: Option<String>,
            _params: Value,
        ) -> Value {
            json!({"ok": true})
        }

        async fn app_version(&self) -> Result<String> {
            Ok("test".to_string())
        }

        async fn update_status(&self) -> Result<UpdateStatus> {
            Ok(UpdateStatus::default())
        }

        async fn check_update(&self) -> Result<Option<UpdateInfo>> {
            Ok(None)
        }

        async fn install_update(&self, _expected_version: &str) -> Result<()> {
            Ok(())
        }

        async fn quit(&self) -> Result<()> {
            Ok(())
        }
    }

    fn fixture() -> (Router, Arc<MockPlatform>, Arc<MockHooks>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::load(temp.path()).unwrap());
        let manager = TaskManager::new(Some(temp.path().to_path_buf()), settings.clone()).unwrap();
        let extension_dir = temp.path().join("extension");
        std::fs::create_dir_all(&extension_dir).unwrap();
        let platform = Arc::new(MockPlatform {
            dir: extension_dir,
            opened_browsers: Arc::new(Mutex::new(Vec::new())),
        });
        let hooks = Arc::new(MockHooks {
            guide_opened: AtomicBool::new(false),
            closed_progress: Arc::new(Mutex::new(Vec::new())),
        });
        let state = ServerState {
            manager,
            settings,
            platform: platform.clone(),
            hooks: hooks.clone(),
            data_dir: temp.path().to_path_buf(),
            duplicate_requests: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            duplicate_seq: Arc::new(AtomicU64::new(1)),
            last_browser_seen: Arc::new(Mutex::new(None)),
            blob_tickets: Arc::new(Mutex::new(HashMap::new())),
        };
        (router(state), platform, hooks, temp)
    }

    fn blob_fixture() -> (Router, ServerState, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::load(temp.path()).unwrap());
        settings
            .update_patch(&json!({"defaultDir": temp.path().to_string_lossy()}))
            .unwrap();
        let manager = TaskManager::new(Some(temp.path().to_path_buf()), settings.clone()).unwrap();
        let platform = Arc::new(MockPlatform {
            dir: temp.path().join("extension"),
            opened_browsers: Arc::new(Mutex::new(Vec::new())),
        });
        let hooks = Arc::new(MockHooks {
            guide_opened: AtomicBool::new(false),
            closed_progress: Arc::new(Mutex::new(Vec::new())),
        });
        let state = ServerState {
            manager,
            settings,
            platform,
            hooks,
            data_dir: temp.path().to_path_buf(),
            duplicate_requests: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            duplicate_seq: Arc::new(AtomicU64::new(1)),
            last_browser_seen: Arc::new(Mutex::new(None)),
            blob_tickets: Arc::new(Mutex::new(HashMap::new())),
        };
        (router(state.clone()), state, temp)
    }

    type TestWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn start_blob_server() -> (String, ServerState, tempfile::TempDir) {
        let (app, state, temp) = blob_fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state, temp)
    }

    async fn prepare_blob(base: &str, filename: &str, size: u64) -> String {
        let response = reqwest::Client::new()
            .post(format!("{base}/api/browser/blob/prepare"))
            .header("X-FDM-Extension", "1")
            .json(&json!({
                "filename": filename,
                "size": size,
                "pageUrl": "https://github.com/acme/repo"
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let value = response.json::<Value>().await.unwrap();
        value["wsUrl"].as_str().unwrap().to_string()
    }

    async fn next_ws_json(ws: &mut TestWs) -> Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                WsMessage::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                WsMessage::Binary(_) => panic!("unexpected binary websocket response"),
                _ => {}
            }
        }
    }

    async fn connect_blob_ws(ws_url: &str) -> (TestWs, Value) {
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        let ready = next_ws_json(&mut ws).await;
        assert_eq!(ready["type"], "ready");
        (ws, ready)
    }

    async fn upload_blob(base: &str, filename: &str, data: &[u8]) -> i64 {
        let ws_url = prepare_blob(base, filename, data.len() as u64).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let task_id = ready["taskId"].as_i64().unwrap();
        for chunk in data.chunks(BLOB_CHUNK_SIZE) {
            ws.send(WsMessage::Binary(chunk.to_vec().into()))
                .await
                .unwrap();
            assert_eq!(next_ws_json(&mut ws).await["type"], "ack");
        }
        ws.send(WsMessage::Text(r#"{"type":"end"}"#.into()))
            .await
            .unwrap();
        assert_eq!(next_ws_json(&mut ws).await["type"], "done");
        task_id
    }

    async fn json_response(app: &Router, mut request: Request<Body>) -> (StatusCode, Value) {
        if !request.headers().contains_key("host") {
            request
                .headers_mut()
                .insert("host", HeaderValue::from_static("127.0.0.1:25432"));
        }
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn extension_endpoints_report_connection_and_open_requests() {
        let (app, platform, hooks, _temp) = fixture();

        let (status, info) = json_response(
            &app,
            Request::builder()
                .uri("/api/system/extension")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["connected"], false);
        assert_eq!(
            info["dir"].as_str().unwrap(),
            platform.dir.to_string_lossy()
        );

        let (status, _) = json_response(
            &app,
            Request::builder()
                .uri("/api/browser/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, info) = json_response(
            &app,
            Request::builder()
                .uri("/api/system/extension")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(info["connected"], true);

        let (status, opened) = json_response(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/system/open_extension_guide")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(opened["opened"], true);
        assert!(hooks.guide_opened.load(Ordering::SeqCst));

        let (status, opened) = json_response(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/system/open_browser_extensions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"browser":"edge"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(opened["opened"], true);
        assert_eq!(
            platform.opened_browsers.lock().unwrap().as_slice(),
            ["edge"]
        );
    }

    #[tokio::test]
    async fn rejects_foreign_origin_and_non_loopback_host() {
        let (app, _platform, _hooks, _temp) = fixture();

        let evil_origin = Request::builder()
            .uri("/api/settings")
            .header("host", "127.0.0.1:25432")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, _) = json_response(&app, evil_origin).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let rebound_host = Request::builder()
            .uri("/api/settings")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, _) = json_response(&app, rebound_host).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let extension = Request::builder()
            .uri("/api/settings")
            .header("host", "127.0.0.1:25432")
            .header("origin", "chrome-extension://abcdefghijklmnop")
            .body(Body::empty())
            .unwrap();
        let (status, value) = json_response(&app, extension).await;
        assert_eq!(status, StatusCode::OK);
        assert!(value.get("defaultDir").is_some());
    }

    #[tokio::test]
    async fn privileged_routes_reject_extension_origin() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .method("POST")
            .uri("/api/system/open_path")
            .header("host", "127.0.0.1:25432")
            .header("origin", "chrome-extension://abcdefghijklmnop")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"path":"C:\\Users\\a\\Downloads"}"#))
            .unwrap();
        let (status, _) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 本机 UI 仍可调用（路径本身是否合法由路径校验决定）
        let ui = Request::builder()
            .method("POST")
            .uri("/api/system/open_path")
            .header("host", "127.0.0.1:25432")
            .header("origin", "http://127.0.0.1:25432")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"path":"C:\\Users\\a\\Downloads\\a.zip"}"#))
            .unwrap();
        let (status, _) = json_response(&app, ui).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn open_path_rejects_unc_and_executables() {
        let (app, _platform, _hooks, _temp) = fixture();
        for path in [
            r"\\evil.example\share\payload.exe",
            r"C:\Users\a\Downloads\setup.exe",
            r"C:\Users\a\Downloads\run.bat",
        ] {
            let body = json!({ "path": path }).to_string();
            let request = Request::builder()
                .method("POST")
                .uri("/api/system/open_path")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let (status, value) = json_response(&app, request).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(value["ok"], false, "应拒绝打开 {path}");
        }
    }

    #[tokio::test]
    async fn probe_torrent_rejects_non_torrent_path() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .method("POST")
            .uri("/api/system/probe_torrent")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"path":"C:\\Users\\a\\secret.txt"}"#))
            .unwrap();
        let (status, value) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["ok"], false);
    }

    #[tokio::test]
    async fn rejects_non_absolute_download_dir() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .method("POST")
            .uri("/api/settings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"defaultDir":"relative/dir"}"#))
            .unwrap();
        let (status, _) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cors_preflight_only_allows_trusted_origins() {
        let (app, _platform, _hooks, _temp) = fixture();

        let evil = Request::builder()
            .method("OPTIONS")
            .uri("/api/settings")
            .header("host", "127.0.0.1:25432")
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(evil).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let extension = Request::builder()
            .method("OPTIONS")
            .uri("/api/settings")
            .header("host", "127.0.0.1:25432")
            .header("origin", "chrome-extension://abcdefghijklmnop")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(extension).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("chrome-extension://abcdefghijklmnop")
        );
    }

    #[tokio::test]
    async fn rejects_cross_site_request_without_origin() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .method("POST")
            .uri("/api/system/quit")
            .header("host", "127.0.0.1:25432")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap();
        let (status, _) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn extension_marker_header_identifies_browser_extension() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .uri("/api/browser/status")
            .header("host", "127.0.0.1:25432")
            .header("origin", "null")
            .header("sec-fetch-site", "cross-site")
            .header("x-fdm-extension", "1")
            .body(Body::empty())
            .unwrap();
        let (status, value) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["online"], true);
    }

    #[tokio::test]
    async fn rejects_relative_task_dir() {
        let (app, _platform, _hooks, _temp) = fixture();
        let request = Request::builder()
            .method("POST")
            .uri("/api/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"url":"https://example.com/x.bin","filename":"x.bin","dir":"relative","start":false}"#,
            ))
            .unwrap();
        let (status, _) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_task_and_remove_all_close_progress_window() {
        let (app, _platform, hooks, temp) = fixture();
        let dir = temp.path().to_string_lossy();

        let create_task_req = |filename: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"url":"https://example.com/{}","filename":"{}","dir":{:?},"start":false}}"#,
                    filename, filename, dir
                )))
                .unwrap()
        };

        let (status, res) = json_response(&app, create_task_req("task1.bin")).await;
        assert_eq!(status, StatusCode::OK);
        let id1 = res["id"].as_i64().unwrap();

        let (status, del_res) = json_response(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/tasks/{}", id1))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(del_res["ok"], true);
        assert!(hooks.closed_progress.lock().unwrap().contains(&id1));

        let (_, res2) = json_response(&app, create_task_req("task2.bin")).await;
        let id2 = res2["id"].as_i64().unwrap();
        let (_, res3) = json_response(&app, create_task_req("task3.bin")).await;
        let id3 = res3["id"].as_i64().unwrap();

        let (status, del_all_res) = json_response(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/tasks/remove_all")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(del_all_res["ok"], true);
        let closed = hooks.closed_progress.lock().unwrap().clone();
        assert!(closed.contains(&id2));
        assert!(closed.contains(&id3));
    }

    #[tokio::test]
    async fn blob_capture_handles_chunk_boundaries_and_consumes_ticket_once() {
        let (base, state, temp) = start_blob_server().await;
        let sizes = [
            0_u64,
            1,
            BLOB_CHUNK_SIZE as u64,
            BLOB_CHUNK_SIZE as u64 + 123,
        ];

        for (index, size) in sizes.into_iter().enumerate() {
            let expected: Vec<u8> = (0..size).map(|value| (value % 251) as u8).collect();
            let filename = format!("blob-{index}.bin");
            let ws_url = prepare_blob(&base, &filename, size).await;
            let (mut ws, ready) = connect_blob_ws(&ws_url).await;
            let task_id = ready["taskId"].as_i64().unwrap();

            for chunk in expected.chunks(BLOB_CHUNK_SIZE) {
                ws.send(WsMessage::Binary(chunk.to_vec().into()))
                    .await
                    .unwrap();
                let ack = next_ws_json(&mut ws).await;
                assert_eq!(ack["type"], "ack");
            }
            ws.send(WsMessage::Text(r#"{"type":"end"}"#.into()))
                .await
                .unwrap();
            let done = next_ws_json(&mut ws).await;
            assert_eq!(done["type"], "done");
            assert_eq!(done["taskId"], task_id);

            let snapshot = state.manager.get_task(task_id).await.unwrap();
            assert_eq!(snapshot.status, "done");
            assert_eq!(std::fs::read(&snapshot.filepath).unwrap(), expected);
            assert!(!std::path::Path::new(&format!("{}.fdmpart", snapshot.filepath)).exists());

            assert!(tokio_tungstenite::connect_async(&ws_url).await.is_err());
        }
        drop(temp);
    }

    #[tokio::test]
    async fn blob_capture_rejects_bad_token_oversize_and_disconnect() {
        let (base, state, temp) = start_blob_server().await;

        assert!(tokio_tungstenite::connect_async(format!(
            "{}/api/browser/blob/ws?token=bogus",
            base.replace("http://", "ws://")
        ))
        .await
        .is_err());

        let ws_url = prepare_blob(&base, "oversize.bin", 5).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let oversize_task = ready["taskId"].as_i64().unwrap();
        ws.send(WsMessage::Binary(vec![0_u8; 6].into()))
            .await
            .unwrap();
        let error = next_ws_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(
            state.manager.get_task(oversize_task).await.unwrap().status,
            "error"
        );

        let ws_url = prepare_blob(&base, "huge-chunk.bin", 8 * 1024 * 1024).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let huge_task = ready["taskId"].as_i64().unwrap();
        // 服务端可能在校验到超大帧时直接断开，因此发送失败也属于预期路径。
        let _ = ws
            .send(WsMessage::Binary(vec![0_u8; BLOB_CHUNK_SIZE + 1].into()))
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let failed = state
                    .manager
                    .get_task(huge_task)
                    .await
                    .map(|task| task.status == "error")
                    .unwrap_or(false);
                if failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        let ws_url = prepare_blob(&base, "disconnect.bin", 10).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let disconnect_task = ready["taskId"].as_i64().unwrap();
        ws.send(WsMessage::Binary(vec![1_u8; 4].into()))
            .await
            .unwrap();
        let ack = next_ws_json(&mut ws).await;
        assert_eq!(ack["type"], "ack");
        drop(ws);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(snapshot) = state.manager.get_task(disconnect_task).await {
                    if snapshot.status == "error" {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        drop(temp);
    }

    #[tokio::test]
    async fn pause_and_resume_are_rejected_for_blob_capture() {
        let (base, state, temp) = start_blob_server().await;
        let data = vec![3_u8; 1024];
        let ws_url = prepare_blob(&base, "pause.bin", data.len() as u64).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let task_id = ready["taskId"].as_i64().unwrap();

        assert!(!state.manager.pause_task(task_id).await);
        assert!(!state.manager.resume_task(task_id).await);

        ws.send(WsMessage::Binary(data.clone().into()))
            .await
            .unwrap();
        assert_eq!(next_ws_json(&mut ws).await["type"], "ack");
        ws.send(WsMessage::Text(r#"{"type":"end"}"#.into()))
            .await
            .unwrap();
        assert_eq!(next_ws_json(&mut ws).await["type"], "done");
        let snapshot = state.manager.get_task(task_id).await.unwrap();
        assert_eq!(snapshot.status, "done");
        assert_eq!(std::fs::read(snapshot.filepath).unwrap(), data);
        drop(temp);
    }

    #[tokio::test]
    async fn removing_blob_task_waits_for_writer_and_cleans_part() {
        let (base, state, temp) = start_blob_server().await;
        let ws_url = prepare_blob(&base, "cancel.bin", 1024 * 1024).await;
        let (mut ws, ready) = connect_blob_ws(&ws_url).await;
        let task_id = ready["taskId"].as_i64().unwrap();
        ws.send(WsMessage::Binary(vec![7_u8; 1024].into()))
            .await
            .unwrap();
        let _ = next_ws_json(&mut ws).await;
        let snapshot = state.manager.get_task(task_id).await.unwrap();
        let part_path = format!("{}.fdmpart", snapshot.filepath);

        assert!(tokio::time::timeout(
            Duration::from_secs(3),
            state.manager.remove_task(task_id, true)
        )
        .await
        .unwrap());
        assert!(state.manager.get_task(task_id).await.is_none());
        assert!(!std::path::Path::new(&part_path).exists());
        drop(temp);
    }

    #[tokio::test]
    async fn blob_ticket_expires_and_duplicate_filename_stays_unique() {
        let (base, state, temp) = start_blob_server().await;

        let expired_url = prepare_blob(&base, "expired.bin", 1).await;
        {
            let mut tickets = state.blob_tickets.lock().unwrap();
            let ticket = tickets.values_mut().next().unwrap();
            ticket.created_at = Instant::now() - BLOB_TICKET_TTL - Duration::from_secs(1);
        }
        assert!(tokio_tungstenite::connect_async(&expired_url)
            .await
            .is_err());

        let first = upload_blob(&base, "same.bin", b"first").await;
        let second = upload_blob(&base, "same.bin", b"second").await;
        let first = state.manager.get_task(first).await.unwrap();
        let second = state.manager.get_task(second).await.unwrap();
        assert_ne!(first.filepath, second.filepath);
        assert_eq!(std::fs::read(first.filepath).unwrap(), b"first");
        assert_eq!(std::fs::read(second.filepath).unwrap(), b"second");
        drop(temp);
    }
}
