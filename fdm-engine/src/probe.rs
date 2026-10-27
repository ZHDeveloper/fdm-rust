use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::anyhow;
use fdm_core::{filename_from_headers_or_url, is_eligible_github_url, Settings, USER_AGENT};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, RANGE, SERVER,
};
use reqwest::Client;

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub supports_range: bool,
    pub total: Option<u64>,
    pub filename: String,
    /// 腾讯 EdgeOne WAF 会拦截并发 Range 请求，只能单连接顺序下载。
    pub edgeone: bool,
}

pub fn build_client(settings: &Settings) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(12))
        // 下载请求不能使用 Client 级总超时，否则长下载会被 30s 截断。
        // 读取停滞由 read_timeout 兜底，探测请求再单独设置总超时。
        .read_timeout(Duration::from_secs(60))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(128)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        // 分块并发若复用同一条 HTTP/2 连接，Google 等 CDN 上多流会互相拖死（只有一个流有数据、其余 0 字节），强制 HTTP/1.1 让每个分块独占 TCP。
        .http1_only()
        .redirect(reqwest::redirect::Policy::limited(10));

    if settings.proxy_enabled && !settings.proxy_host.trim().is_empty() {
        let scheme = if settings.proxy_type.trim().is_empty() {
            "http"
        } else {
            settings.proxy_type.trim()
        };
        let port = if settings.proxy_port.trim().is_empty() {
            String::new()
        } else {
            format!(":{}", settings.proxy_port.trim())
        };
        let url = format!("{}://{}{}", scheme, settings.proxy_host.trim(), port);
        let mut proxy = reqwest::Proxy::all(&url)?;
        if !settings.proxy_user.is_empty() {
            proxy = proxy.basic_auth(&settings.proxy_user, &settings.proxy_pass);
        }
        builder = builder.proxy(proxy);
    }
    Ok(builder.build()?)
}

fn client_key(settings: &Settings) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        settings.proxy_enabled,
        settings.proxy_type,
        settings.proxy_host,
        settings.proxy_port,
        settings.proxy_user,
        settings.proxy_pass
    )
}

/// 全局 HTTP 客户端缓存：代理配置不变时，所有探测、分片、单流和种子抓取复用同一连接池。
#[derive(Default)]
pub struct HttpClientCache {
    cached: RwLock<Option<(String, Arc<Client>)>>,
}

impl HttpClientCache {
    pub fn get(&self, settings: &Settings) -> anyhow::Result<Arc<Client>> {
        let key = client_key(settings);
        if let Ok(cached) = self.cached.read() {
            if let Some((cached_key, client)) = cached.as_ref() {
                if cached_key == &key {
                    return Ok(client.clone());
                }
            }
        }
        let client = Arc::new(build_client(settings)?);
        if let Ok(mut cached) = self.cached.write() {
            *cached = Some((key, client.clone()));
        }
        Ok(client)
    }
}

pub fn headers_map(custom: &HashMap<String, String>) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (key, value) in custom {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, val);
        }
    }
    map
}

async fn probe_inner(
    client: &Client,
    url: &str,
    custom: &HashMap<String, String>,
    existing_filename: &str,
    verify_range: bool,
) -> anyhow::Result<ProbeResult> {
    let headers = headers_map(custom);
    let mut supports_range = false;
    let mut total: Option<u64> = None;
    let mut edgeone = false;
    let mut resolved_headers = HeaderMap::new();

    if let Ok(resp) = client
        .head(url)
        .headers(headers.clone())
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        edgeone = is_edgeone_waf(resp.status().as_u16(), resp.headers());
        if resp.status().as_u16() < 400 {
            resolved_headers = resp.headers().clone();
            if !is_html_error(url, &resolved_headers) {
                let accept_ranges = header_text(&resolved_headers, ACCEPT_RANGES);
                supports_range = accept_ranges.to_ascii_lowercase().contains("bytes");
                total = parse_content_length(&resolved_headers);
            }
        }
    }

    // 只有启用 GitHub 加速（多候选）时才强制用 Range 0-0 验证镜像；
    // 普通单地址下载保持原有“HEAD 足够时不再发 GET”的行为，避免消耗一次性链接。
    if verify_range || total.is_none() || !supports_range {
        if let Ok(resp) = client
            .get(url)
            .headers(headers.clone())
            .header(RANGE, "bytes=0-0")
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            let status = resp.status().as_u16();
            let get_headers = resp.headers().clone();
            edgeone |= is_edgeone_waf(status, &get_headers);
            if is_html_error(url, &get_headers) {
                // GitHub 二进制下载被镜像换成了 HTML 错误页。
                total = None;
                supports_range = false;
            } else if status == 206 {
                if let Some((start, end, range_total)) = parse_content_range(&get_headers) {
                    if start == 0
                        && end == 0
                        && range_total > 0
                        && total
                            .map(|head_total| head_total == range_total)
                            .unwrap_or(true)
                    {
                        supports_range = true;
                        total = Some(range_total);
                        merge_content_disposition(&mut resolved_headers, &get_headers);
                    } else {
                        total = None;
                        supports_range = false;
                    }
                } else {
                    // 206 必须带合法的 Content-Range，否则不能当作可续传响应。
                    total = None;
                    supports_range = false;
                }
            } else if status == 200 {
                let get_total = parse_content_length(&get_headers);
                if let (Some(head_total), Some(get_total)) = (total, get_total) {
                    if head_total != get_total {
                        // 镜像返回的 HTML 错误页：清掉大小，让 probe_candidates 继续换下一个。
                        total = None;
                        supports_range = false;
                    } else {
                        supports_range = false;
                        merge_content_disposition(&mut resolved_headers, &get_headers);
                    }
                } else {
                    if get_total.is_some() {
                        total = get_total;
                    }
                    supports_range = false;
                    merge_content_disposition(&mut resolved_headers, &get_headers);
                }
            } else if status >= 400 {
                // 单候选时保持原有行为：不把探测失败直接放大成任务失败；
                // 多候选时由 probe_candidates 根据 total 为空自动切换。
                total = None;
                supports_range = false;
            }
        }
    }

    let content_disposition = resolved_headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok());
    let filename = if existing_filename.trim().is_empty() {
        filename_from_headers_or_url(url, content_disposition)
    } else {
        existing_filename.to_string()
    };

    Ok(ProbeResult {
        supports_range,
        total,
        filename,
        edgeone,
    })
}

/// 按顺序探测候选地址，返回第一个可用结果及其索引。
///
/// 多候选（即启用了 GitHub 加速）时，单个候选最多等待 10 秒；
/// 单候选时沿用原来的探测超时，避免影响非 GitHub 下载。
pub async fn probe_candidates(
    client: &Client,
    candidates: &[String],
    custom: &HashMap<String, String>,
    existing_filename: &str,
) -> anyhow::Result<(ProbeResult, usize)> {
    let mut last_error: Option<anyhow::Error> = None;
    let mut fallback: Option<(ProbeResult, usize)> = None;

    for (index, url) in candidates.iter().enumerate() {
        let future = probe_inner(client, url, custom, existing_filename, candidates.len() > 1);
        let outcome = if candidates.len() > 1 {
            match tokio::time::timeout(Duration::from_secs(10), future).await {
                Ok(result) => result,
                Err(_) => Err(anyhow!("探测超时: {url}")),
            }
        } else {
            future.await
        };

        match outcome {
            Ok(result) => {
                if result.total.is_some() || result.supports_range {
                    if index > 0 {
                        tracing::info!(candidate = index, url = %url, "GitHub 加速：候选地址探测成功");
                    }
                    return Ok((result, index));
                }
                if fallback.is_none() {
                    fallback = Some((result, index));
                }
            }
            Err(err) => {
                tracing::warn!(candidate = index, url = %url, error = %err, "候选地址探测失败");
                last_error = Some(err);
            }
        }
    }

    if let Some(result) = fallback {
        return Ok(result);
    }
    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的下载地址")))
}

/// 依次尝试候选地址抓取种子文件，并要求响应是 bencode 字典（以 `d` 开头）。
pub async fn fetch_torrent_bytes_candidates(
    client: &Client,
    candidates: &[String],
) -> anyhow::Result<Vec<u8>> {
    let mut last_error: Option<anyhow::Error> = None;
    for (index, url) in candidates.iter().enumerate() {
        let outcome: anyhow::Result<Vec<u8>> = match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) if bytes.first() == Some(&b'd') => return Ok(bytes.to_vec()),
                Ok(_) => Err(anyhow!("响应不是种子文件: {url}")),
                Err(err) => Err(err.into()),
            },
            Ok(resp) => Err(anyhow!("HTTP {}: {url}", resp.status())),
            Err(err) => Err(err.into()),
        };
        if let Err(err) = outcome {
            tracing::warn!(candidate = index, url = %url, error = %err, "种子文件候选地址失败");
            last_error = Some(err);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的种子文件地址")))
}

/// 腾讯 EdgeOne WAF：返回 567，或带 `EO-*` / `Server: EdgeOne` 响应头。
fn is_edgeone_waf(status: u16, headers: &HeaderMap) -> bool {
    status == 567
        || headers
            .keys()
            .any(|name| name.as_str().to_ascii_lowercase().starts_with("eo-"))
        || header_text(headers, SERVER)
            .to_ascii_lowercase()
            .contains("edgeone")
}

fn header_text(headers: &HeaderMap, name: HeaderName) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn is_html_error(url: &str, headers: &HeaderMap) -> bool {
    if !is_eligible_github_url(url) {
        return false;
    }
    let is_binary_path = url::Url::parse(url)
        .ok()
        .map(|parsed| {
            let path = parsed.path().to_ascii_lowercase();
            path.contains("/releases/download/")
                || path.contains("/archive/")
                || path.ends_with(".zip")
                || path.ends_with(".tar.gz")
                || path.ends_with(".tgz")
                || path.ends_with(".7z")
                || path.ends_with(".exe")
                || path.ends_with(".msi")
                || path.ends_with(".dmg")
                || path.ends_with(".pkg")
                || path.ends_with(".deb")
                || path.ends_with(".rpm")
        })
        .unwrap_or(false);
    if !is_binary_path {
        return false;
    }
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .to_ascii_lowercase()
                .trim_start()
                .starts_with("text/html")
        })
        .unwrap_or(false)
}

fn merge_content_disposition(target: &mut HeaderMap, source: &HeaderMap) {
    if target.get(CONTENT_DISPOSITION).is_none() {
        if let Some(value) = source.get(CONTENT_DISPOSITION) {
            target.insert(CONTENT_DISPOSITION, value.clone());
        }
    }
}

fn parse_content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let lower = value.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("bytes ")?;
    let (range_part, total_part) = rest.split_once('/')?;
    let (start, end) = range_part.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total_part.trim().parse().ok()?,
    ))
}

fn parse_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_error_for_github_binary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        assert!(is_html_error(
            "https://github.com/a/b/releases/download/v1/x.zip",
            &headers
        ));
        assert!(!is_html_error("https://github.com/a/b", &headers));
    }

    #[test]
    fn detects_edgeone_waf() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("eo-log-uuid"),
            HeaderValue::from_static("x"),
        );
        assert!(is_edgeone_waf(200, &headers));
        let mut server = HeaderMap::new();
        server.insert(SERVER, HeaderValue::from_static("EdgeOne"));
        assert!(is_edgeone_waf(200, &server));
        assert!(is_edgeone_waf(567, &HeaderMap::new()));
        assert!(!is_edgeone_waf(200, &HeaderMap::new()));
    }

    #[test]
    fn parses_content_range() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/123"));
        assert_eq!(parse_content_range(&headers), Some((0, 0, 123)));
    }
}
