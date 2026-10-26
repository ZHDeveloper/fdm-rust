use percent_encoding::percent_decode_str;

/// 已解析的 magnet 链接信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MagnetInfo {
    pub btih: Option<String>,
    pub ed2k_hash: Option<String>,
    pub name: Option<String>,
    pub size: Option<u64>,
}

/// 已解析的 ed2k 文件链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ed2kLink {
    pub name: String,
    pub size: u64,
    pub hash: String,
    pub aich: Option<String>,
}

/// 按 URL 推断任务类型：http / bt / ed2k。
pub fn detect_kind(url: &str) -> &'static str {
    let value = url.trim();
    if value.is_empty() {
        return "http";
    }
    if value.starts_with("ed2k://") {
        return "ed2k";
    }
    if value.starts_with("magnet:") {
        return match parse_magnet(value) {
            Some(info) if info.ed2k_hash.is_some() && info.btih.is_none() => "ed2k",
            _ => "bt",
        };
    }
    if is_torrent_url(value) {
        return "bt";
    }
    "http"
}

/// 是否为以 .torrent 结尾的 HTTP(S) 链接（忽略查询串与片段）。
pub fn is_torrent_url(url: &str) -> bool {
    let value = url.trim();
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return false;
    }
    let without_fragment = value.split('#').next().unwrap_or(value);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query.to_ascii_lowercase().ends_with(".torrent")
}

/// 解析 magnet 链接。支持 urn:btih（40 位 hex / 32 位 base32）与 urn:ed2k。
pub fn parse_magnet(link: &str) -> Option<MagnetInfo> {
    let rest = link.trim().strip_prefix("magnet:?")?;
    let mut info = MagnetInfo::default();
    for pair in rest.split('&') {
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        let value = decode_component(raw_value);
        match key {
            "xt" => {
                if let Some(hash) = value.strip_prefix("urn:btih:") {
                    if info.btih.is_none() {
                        info.btih = normalize_btih(hash);
                    }
                } else if let Some(hash) = value.strip_prefix("urn:ed2k:") {
                    if info.ed2k_hash.is_none() {
                        let hash = hash.trim().to_ascii_lowercase();
                        if is_hex32(&hash) {
                            info.ed2k_hash = Some(hash);
                        }
                    }
                }
            }
            "dn" => {
                if info.name.is_none() && !value.trim().is_empty() {
                    info.name = Some(value.trim().to_string());
                }
            }
            "xl" => {
                if info.size.is_none() {
                    if let Ok(size) = value.trim().parse::<u64>() {
                        if size > 0 {
                            info.size = Some(size);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if info.btih.is_none() && info.ed2k_hash.is_none() && info.name.is_none() && info.size.is_none()
    {
        return None;
    }
    Some(info)
}

/// 解析 ed2k 文件链接：ed2k://|file|<name>|<size>|<hash>|[|h=<aich>|]/
pub fn parse_ed2k_link(link: &str) -> Option<Ed2kLink> {
    let rest = link.trim().strip_prefix("ed2k://")?.trim_start_matches('|');
    let parts: Vec<&str> = rest.split('|').collect();
    if parts.first().copied()? != "file" {
        return None;
    }
    let name = parts.get(1).copied()?.trim();
    let size = parts.get(2).copied()?.trim().parse::<u64>().ok()?;
    let hash = parts.get(3).copied()?.trim().to_ascii_lowercase();
    if name.is_empty() || !is_hex32(&hash) {
        return None;
    }
    let aich = parts
        .iter()
        .find_map(|part| part.strip_prefix("h="))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| is_hex32(s));
    Some(Ed2kLink {
        name: name.to_string(),
        size,
        hash,
        aich,
    })
}

fn decode_component(value: &str) -> String {
    let replaced = value.replace('+', " ");
    percent_decode_str(&replaced)
        .decode_utf8_lossy()
        .into_owned()
}

/// 将 40 位十六进制或 32 位 base32 的 btih 统一成小写十六进制。
pub fn normalize_btih(value: &str) -> Option<String> {
    let value = value.trim();
    if is_hex32(value) {
        return Some(value.to_ascii_lowercase());
    }
    if value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(value.to_ascii_lowercase());
    }
    let bytes = decode_base32(value)?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = String::with_capacity(40);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Some(out)
}

fn is_hex32(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn decode_base32(value: &str) -> Option<Vec<u8>> {
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(value.len() * 5 / 8);
    for ch in value.trim_end_matches('=').chars() {
        let v = match ch.to_ascii_uppercase() {
            'A'..='Z' => ch.to_ascii_uppercase() as u32 - 'A' as u32,
            '2'..='7' => ch as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | v;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_protocols() {
        assert_eq!(detect_kind("https://example.com/a.bin"), "http");
        assert_eq!(detect_kind("https://example.com/a.torrent?x=1"), "bt");
        assert_eq!(
            detect_kind("magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862"),
            "bt"
        );
        assert_eq!(
            detect_kind("magnet:?xt=urn:ed2k:8b54a3c20fae9e4b9f7e0c2c8c01b6b1&xl=123"),
            "ed2k"
        );
        assert_eq!(
            detect_kind("ed2k://|file|a.iso|123|8b54a3c20fae9e4b9f7e0c2c8c01b6b1|/"),
            "ed2k"
        );
    }

    #[test]
    fn parses_ed2k_links() {
        let link = "ed2k://|file|ubuntu.iso|3825205248|8B54A3C20FAE9E4B9F7E0C2C8C01B6B1|h=0123456789ABCDEF0123456789ABCDEF|/";
        let parsed = parse_ed2k_link(link).unwrap();
        assert_eq!(parsed.name, "ubuntu.iso");
        assert_eq!(parsed.size, 3825205248);
        assert_eq!(parsed.hash, "8b54a3c20fae9e4b9f7e0c2c8c01b6b1");
        assert_eq!(
            parsed.aich.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(parse_ed2k_link("ed2k://|server|1.2.3.4|4661|/").is_none());
        assert!(parse_ed2k_link("ed2k://|file|a|0|bad|/").is_none());
    }

    #[test]
    fn parses_magnets() {
        let hex = parse_magnet(
            "magnet:?xt=urn:btih:CAB507494D02EBB1178B38F2E9D7BE299C86B862&dn=Ubuntu&xl=123",
        )
        .unwrap();
        assert_eq!(
            hex.btih.as_deref(),
            Some("cab507494d02ebb1178b38f2e9d7be299c86b862")
        );
        assert_eq!(hex.name.as_deref(), Some("Ubuntu"));
        assert_eq!(hex.size, Some(123));

        // 32 位 base32 btih
        let base32 = parse_magnet("magnet:?xt=urn:btih:ZFQ6SSUD2LMBE4X3F4T3Z7TVZH5ZWSD2").unwrap();
        assert!(base32.btih.is_some());

        let ed2k =
            parse_magnet("magnet:?xt=urn:ed2k:8b54a3c20fae9e4b9f7e0c2c8c01b6b1&xl=42").unwrap();
        assert_eq!(
            ed2k.ed2k_hash.as_deref(),
            Some("8b54a3c20fae9e4b9f7e0c2c8c01b6b1")
        );
    }
}
