use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_TYPE,
};
use reqwest::{Client, Method};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::ServerState;

const PROXY_ADDR: &str = "127.0.0.1:25433";
const MAX_HEAD: usize = 128 * 1024;
const MAX_REQUEST_BODY: usize = 128 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
static PROXY_READY: AtomicBool = AtomicBool::new(false);
static CLIENT: OnceLock<Client> = OnceLock::new();

pub fn is_ready() -> bool {
    PROXY_READY.load(Ordering::Relaxed)
}

fn client(settings: &fdm_core::Settings) -> &'static Client {
    CLIENT.get_or_init(|| {
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(300))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
            .http1_only();
        if settings.proxy_enabled && !settings.proxy_host.trim().is_empty() {
            let scheme = match settings.proxy_type.trim() {
                "" => "http",
                other => other,
            };
            let host = settings.proxy_host.trim();
            let port = settings.proxy_port.trim();
            let addr = if port.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}:{port}")
            };
            if let Ok(mut proxy) = reqwest::Proxy::all(addr) {
                if !settings.proxy_user.is_empty() {
                    proxy = proxy.basic_auth(&settings.proxy_user, &settings.proxy_pass);
                }
                builder = builder.proxy(proxy);
            }
        }
        builder.build().expect("构建本地代理 HTTP 客户端失败")
    })
}

/// 用户态 HTTP 数据面：浏览器把原始请求发给这里，代理透明转发并观察响应。
///
/// 这不是按 URL 重放：请求由浏览器发起，代理只是位于浏览器和目标之间，
/// 因此一次性表单 session、Cookie、上传 body 都保持原始语义。
pub async fn run(state: ServerState) -> Result<()> {
    let listener = TcpListener::bind(PROXY_ADDR)
        .await
        .with_context(|| format!("绑定本地代理 {PROXY_ADDR} 失败"))?;
    PROXY_READY.store(true, Ordering::Relaxed);
    loop {
        let (socket, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(socket, state).await {
                tracing::warn!(error = %err, "本地代理请求失败");
            }
        });
    }
}

async fn handle(mut socket: TcpStream, state: ServerState) -> Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };
    if head.starts_with(b"CONNECT ") {
        return write_status(&mut socket, 501, "CONNECT not supported").await;
    }
    let request = parse_request(&head, body)?;
    if request.url.scheme() != "http" || is_loopback_host(request.url.host_str().unwrap_or("")) {
        return write_status(&mut socket, 400, "Bad Request").await;
    }

    let mut outgoing = HeaderMap::new();
    for (key, value) in &request.headers {
        let lower = key.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "connection"
                | "proxy-connection"
                | "proxy-authorization"
                | "keep-alive"
                | "content-length"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
        ) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            outgoing.insert(name, val);
        }
    }
    // 代理需要完整原始字节，不能让上游按浏览器协商结果压缩。
    outgoing.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    let settings = state.settings.get();
    let mut req = client(&settings)
        .request(request.method.clone(), request.url.clone())
        .headers(outgoing);
    if !request.body.is_empty() || request.method == Method::POST {
        req = req.body(request.body);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(error = %err, url = %request.url, "代理转发失败");
            return write_status(&mut socket, 502, "Bad Gateway").await;
        }
    };
    let status = resp.status().as_u16();
    let response_headers = resp.headers().clone();

    if should_capture(status, &response_headers, &request.method) {
        let disposition = response_headers
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok());
        let filename = fdm_core::filename_from_headers_or_url(request.url.as_str(), disposition);
        let total = response_headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        match capture_body(&state, &request.url, &filename, total, resp).await {
            Ok(()) => return write_status(&mut socket, 204, "No Content").await,
            Err(err) => {
                tracing::warn!(error = %err, url = %request.url, "代理捕获下载失败");
                return write_status(&mut socket, 502, "Bad Gateway").await;
            }
        }
    }

    write_streaming_response(
        &mut socket,
        status,
        &response_headers,
        resp,
        request.method == Method::HEAD,
    )
    .await
}

fn should_capture(status: u16, headers: &HeaderMap, method: &Method) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if disposition.contains("attachment") || disposition.contains("filename=") {
        return true;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        content_type.as_str(),
        "application/x-targz"
            | "application/x-gzip"
            | "application/zip"
            | "application/x-tar"
            | "application/octet-stream"
    ) {
        return true;
    }
    // POST 表单本身就可能是“生成并下载”的接口（路由器备份等），
    // 非文本响应即可视为文件，避免浏览器再下一次而卡在下载决策。
    if *method == Method::POST {
        return !(content_type.is_empty()
            || content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("javascript"));
    }
    false
}

async fn capture_body(
    state: &ServerState,
    url: &Url,
    filename: &str,
    total: Option<u64>,
    resp: reqwest::Response,
) -> Result<()> {
    let (id, part_path) = state
        .manager
        .begin_capture(url.as_str(), filename, total)
        .await?;
    // 与浏览器自动接管保持一致：自动下载时立刻弹出进度窗。
    if state.settings.get().auto_download {
        if let Err(err) = state.hooks.open_progress(id).await {
            tracing::warn!(error = %err, "打开下载进度窗口失败");
        }
    }
    let mut file = match tokio::fs::File::create(&part_path).await {
        Ok(file) => file,
        Err(err) => {
            let err = anyhow::Error::from(err).context(format!("创建 {}", part_path.display()));
            state.manager.fail_capture(id, &err.to_string()).await;
            return Err(err);
        }
    };
    let max_rate = state.settings.get().max_rate_kb.saturating_mul(1024);
    let started = Instant::now();
    let mut written = 0u64;
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let mut last_sample = Instant::now();
    let mut last_bytes = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        // 暂停/删除后立即停止读取上游，避免“已暂停但还在下载”。
        if state.manager.capture_should_stop(id).await {
            let _ = file.flush().await;
            drop(file);
            // 暂停保留 .fdmpart；任务被删除则顺手清理半成品。
            if !state.manager.capture_exists(id).await {
                let _ = tokio::fs::remove_file(&part_path).await;
            }
            return Ok(());
        }
        // 250ms 超时只为重新检查暂停/删除，不中断正常读取。
        let chunk = match tokio::time::timeout(Duration::from_millis(250), stream.next()).await {
            Err(_) => continue,
            Ok(None) => break,
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(err))) => {
                drop(file);
                state.manager.fail_capture(id, &err.to_string()).await;
                return Err(err.into());
            }
        };
        if let Err(err) = file.write_all(&chunk).await {
            drop(file);
            state.manager.fail_capture(id, &err.to_string()).await;
            return Err(err.into());
        }
        written += chunk.len() as u64;
        if max_rate > 0 {
            // 捕获走浏览器真实请求，限速只能靠回压上游读取。
            let target = written as f64 / max_rate as f64;
            let elapsed = started.elapsed().as_secs_f64();
            if target > elapsed {
                tokio::time::sleep(Duration::from_secs_f64(target - elapsed)).await;
            }
        }
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            let dt = last_sample.elapsed().as_secs_f64().max(0.001);
            let speed = (written - last_bytes) as f64 / dt;
            let _ = state
                .manager
                .update_capture(id, written, total, speed)
                .await;
            last_bytes = written;
            last_sample = Instant::now();
            last_emit = Instant::now();
        }
    }
    if let Err(err) = file.flush().await {
        drop(file);
        state.manager.fail_capture(id, &err.to_string()).await;
        return Err(err.into());
    }
    drop(file);
    state.manager.finish_capture(id, written).await?;
    Ok(())
}

struct RequestParts {
    method: Method,
    url: Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn parse_request(head: &[u8], body: Vec<u8>) -> Result<RequestParts> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| anyhow!("缺少请求行"))?;
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").parse::<Method>()?;
    let target = parts.next().unwrap_or("");
    let url = Url::parse(target).context("代理请求必须是绝对 URL")?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(RequestParts {
        method,
        url,
        headers,
        body,
    })
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

async fn read_request(socket: &mut TcpStream) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut data = Vec::new();
    let head_end = loop {
        let mut buf = [0u8; 8192];
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() > MAX_HEAD {
            return Err(anyhow!("请求头过大"));
        }
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = data[..head_end].to_vec();
    let mut body = data[head_end + 4..].to_vec();
    let text = String::from_utf8_lossy(&head);
    let content_length = text.lines().find_map(|line| {
        line.split_once(':').and_then(|(key, value)| {
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
    });
    if let Some(length) = content_length {
        if length > MAX_REQUEST_BODY {
            return Err(anyhow!("请求体过大"));
        }
        while body.len() < length {
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Err(anyhow!("请求体提前结束"));
            }
            body.extend_from_slice(&buf[..n]);
        }
        body.truncate(length);
    } else if text.lines().any(|line| {
        line.split_once(':')
            .map(|(key, value)| {
                key.eq_ignore_ascii_case("transfer-encoding")
                    && value.to_ascii_lowercase().contains("chunked")
            })
            .unwrap_or(false)
    }) {
        // 表单通常带 Content-Length；这里兜底 chunked 请求体。
        body = read_chunked_body(socket, body).await?;
    }
    Ok(Some((head, body)))
}

async fn read_chunked_body(socket: &mut TcpStream, mut data: Vec<u8>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        while data.len() < 2 {
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Err(anyhow!("chunked 请求体提前结束"));
            }
            data.extend_from_slice(&buf[..n]);
        }
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("chunked 长度行非法"))?;
        let line = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)?;
        data.drain(..line_end + 2);
        if size == 0 {
            return Ok(out);
        }
        while data.len() < size + 2 {
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Err(anyhow!("chunked 请求体提前结束"));
            }
            data.extend_from_slice(&buf[..n]);
        }
        out.extend_from_slice(&data[..size]);
        data.drain(..size + 2);
    }
}

async fn write_status(socket: &mut TcpStream, status: u16, reason: &str) -> Result<()> {
    let text =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    socket.write_all(text.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

async fn write_streaming_response(
    socket: &mut TcpStream,
    status: u16,
    headers: &HeaderMap,
    resp: reqwest::Response,
    head_only: bool,
) -> Result<()> {
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("OK");
    let mut text = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection" | "keep-alive" | "transfer-encoding"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            text.push_str(name.as_str());
            text.push_str(": ");
            text.push_str(value);
            text.push_str("\r\n");
        }
    }
    text.push_str("Connection: close\r\n\r\n");
    socket.write_all(text.as_bytes()).await?;
    if !head_only {
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            socket.write_all(&chunk?).await?;
        }
    }
    socket.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_form_proxy_request() {
        let head = b"POST http://192.168.1.1/cgi-bin/cgi-backup HTTP/1.1\r\nHost: 192.168.1.1\r\nContent-Length: 3\r\n\r\n";
        let req = parse_request(head, b"x=1".to_vec()).unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.url.as_str(), "http://192.168.1.1/cgi-bin/cgi-backup");
        assert_eq!(req.body, b"x=1");
    }

    #[test]
    fn rejects_origin_form_request() {
        let head = b"GET /index.html HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert!(parse_request(head, Vec::new()).is_err());
    }

    #[test]
    fn detects_attachment_and_binary_downloads() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"a.tar.gz\""),
        );
        assert!(should_capture(200, &headers, &Method::GET));
        assert!(!should_capture(403, &headers, &Method::GET));

        let mut binary = HeaderMap::new();
        binary.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-targz"),
        );
        assert!(should_capture(200, &binary, &Method::GET));

        let mut html = HeaderMap::new();
        html.insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        assert!(!should_capture(200, &html, &Method::GET));
    }
}
