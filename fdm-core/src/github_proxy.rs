use std::collections::HashMap;

use url::Url;

use crate::settings::Settings;

/// 内置 GitHub 下载加速前缀，按推荐顺序排列。
pub const GITHUB_PROXY_PRESETS: [&str; 3] = [
    "https://ghproxy.felicity.land/",
    "https://gh-proxy.com/",
    "https://xsadwsd.kdns.fr/",
];

/// 规范化用户填写的代理前缀：仅接受 http/https，并保证以 `/` 结尾。
pub fn normalize_prefix(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return None;
    }
    let parsed = Url::parse(raw).ok()?;
    parsed.host_str()?;
    let mut value = parsed.to_string();
    if !value.ends_with('/') {
        value.push('/');
    }
    Some(value)
}

/// 只对 GitHub 的发布下载和 raw 文件启用加速；其他域名保持直连。
pub fn is_eligible_github_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return false;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    matches!(
        parsed
            .host_str()
            .map(|host| host.to_ascii_lowercase())
            .as_deref(),
        Some("github.com") | Some("www.github.com") | Some("raw.githubusercontent.com")
    )
}

/// 带鉴权信息的请求不允许交给第三方镜像。
pub fn has_sensitive_headers(headers: &HashMap<String, String>) -> bool {
    headers.keys().any(|key| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "authorization" | "cookie" | "proxy-authorization" | "x-github-token"
        )
    })
}

fn has_sensitive_query(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return false;
    };
    parsed.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "token" | "access_token" | "private_token" | "x-amz-signature"
        )
    })
}

/// 返回按优先级排列的请求地址；最后一项可能是原始直连地址。
pub fn github_proxy_candidates(
    url: &str,
    settings: &Settings,
    sensitive_headers: bool,
) -> Vec<String> {
    let url = url.trim();
    if !settings.github_proxy_enabled
        || sensitive_headers
        || has_sensitive_query(url)
        || !is_eligible_github_url(url)
    {
        return vec![url.to_string()];
    }

    let mut prefixes: Vec<String> = Vec::new();
    if let Some(custom) = normalize_prefix(&settings.github_proxy_url) {
        prefixes.push(custom);
    }
    for preset in GITHUB_PROXY_PRESETS {
        let preset = preset.to_string();
        if !prefixes
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&preset))
        {
            prefixes.push(preset);
        }
    }

    let mut candidates: Vec<String> = prefixes
        .into_iter()
        .map(|prefix| format!("{prefix}{url}"))
        .collect();
    if settings.github_proxy_fallback_direct {
        candidates.push(url.to_string());
    }
    if candidates.is_empty() {
        candidates.push(url.to_string());
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_settings() -> Settings {
        Settings {
            github_proxy_enabled: true,
            github_proxy_fallback_direct: true,
            ..Settings::default()
        }
    }

    #[test]
    fn disabled_proxy_keeps_original_url() {
        let settings = Settings {
            github_proxy_enabled: false,
            ..Settings::default()
        };
        let url = "https://github.com/a/b/releases/download/v1/x.zip";
        assert_eq!(github_proxy_candidates(url, &settings, false), vec![url]);
    }

    #[test]
    fn auto_mode_uses_presets_then_direct() {
        let settings = enabled_settings();
        let url = "https://github.com/a/b/releases/download/v1/x.zip";
        let out = github_proxy_candidates(url, &settings, false);
        assert_eq!(out.len(), GITHUB_PROXY_PRESETS.len() + 1);
        assert_eq!(out[0], format!("{}{}", GITHUB_PROXY_PRESETS[0], url));
        assert_eq!(out[1], format!("{}{}", GITHUB_PROXY_PRESETS[1], url));
        assert_eq!(out.last().map(String::as_str), Some(url));
    }

    #[test]
    fn direct_fallback_can_be_disabled() {
        let mut settings = enabled_settings();
        settings.github_proxy_fallback_direct = false;
        let url = "https://raw.githubusercontent.com/a/b/master/README.md";
        let out = github_proxy_candidates(url, &settings, false);
        assert_eq!(out.len(), GITHUB_PROXY_PRESETS.len());
        assert!(out.iter().all(|item| item != url));
    }

    #[test]
    fn custom_prefix_is_first_and_presets_remain_as_fallbacks() {
        let mut settings = enabled_settings();
        settings.github_proxy_url = "https://ghfast.top".to_string();
        let url = "https://github.com/a/b/archive/refs/heads/main.zip";
        let out = github_proxy_candidates(url, &settings, false);
        assert_eq!(out[0], format!("https://ghfast.top/{url}"));
        assert_eq!(out.len(), GITHUB_PROXY_PRESETS.len() + 2);
    }

    #[test]
    fn invalid_custom_prefix_falls_back_to_auto_list() {
        let mut settings = enabled_settings();
        settings.github_proxy_url = "not-a-url".to_string();
        let url = "https://github.com/a/b/archive/refs/heads/main.zip";
        let out = github_proxy_candidates(url, &settings, false);
        assert_eq!(out.len(), GITHUB_PROXY_PRESETS.len() + 1);
        assert_eq!(out[0], format!("{}{}", GITHUB_PROXY_PRESETS[0], url));
    }

    #[test]
    fn non_github_url_is_not_rewritten() {
        let settings = enabled_settings();
        let url = "https://example.com/file.zip";
        assert_eq!(github_proxy_candidates(url, &settings, false), vec![url]);
    }

    #[test]
    fn sensitive_headers_or_query_disable_proxy() {
        let settings = enabled_settings();
        let url = "https://github.com/a/b/releases/download/v1/x.zip";
        assert_eq!(github_proxy_candidates(url, &settings, true), vec![url]);
        let signed = "https://github.com/a/b/archive.zip?access_token=secret";
        assert_eq!(
            github_proxy_candidates(signed, &settings, false),
            vec![signed]
        );
    }

    #[test]
    fn normalized_prefix_gets_trailing_slash() {
        assert_eq!(
            normalize_prefix(" https://ghfast.top "),
            Some("https://ghfast.top/".to_string())
        );
        assert!(normalize_prefix("ghfast.top").is_none());
    }
}
