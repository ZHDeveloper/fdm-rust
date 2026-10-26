use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{absolute_path, path_to_string, Result};
use crate::paths;

pub const CHUNK_SIZE: usize = 64 * 1024;
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_CONNECTING: &str = "connecting";
pub const STATUS_WAITING: &str = "waiting";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_MERGING: &str = "merging";
pub const STATUS_PAUSED: &str = "paused";
pub const STATUS_DONE: &str = "done";
pub const STATUS_ERROR: &str = "error";
pub const STATUS_SEEDING: &str = "seeding";

pub fn is_active_status(status: &str) -> bool {
    matches!(
        status,
        STATUS_CONNECTING | STATUS_WAITING | STATUS_DOWNLOADING | STATUS_MERGING
    )
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(status, STATUS_DONE | STATUS_PAUSED | STATUS_ERROR)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Segment {
    pub index: usize,
    pub start: u64,
    pub end: u64,
    #[serde(default)]
    pub cur: u64,
    #[serde(default)]
    pub done: bool,
    /// 运行期连接归属；旧 .fdmcfg 无该字段时为 None。
    #[serde(default)]
    pub worker: Option<usize>,
}

impl Segment {
    pub fn new(index: usize, start: u64, end: u64) -> Self {
        Self {
            index,
            start,
            end,
            cur: start,
            done: false,
            worker: None,
        }
    }

    pub fn downloaded(&self) -> u64 {
        if self.cur < self.start {
            return 0;
        }
        (self.cur - self.start).min(self.end.saturating_sub(self.start) + 1)
    }

    pub fn total(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }
    pub fn complete(&self) -> bool {
        self.done || self.cur > self.end
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TorrentFileInfo {
    pub index: usize,
    pub path: String,
    pub length: u64,
    #[serde(default)]
    pub progress: u64,
    #[serde(default = "default_true")]
    pub selected: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub id: i64,
    pub url: String,
    pub filename: String,
    pub dir: String,
    pub filepath: String,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub progress: f64,
    pub speed: u64,
    pub status: String,
    pub error: Option<String>,
    pub kind: String,
    pub connections: usize,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub info_hash: Option<String>,
    #[serde(default)]
    pub ed2k_hash: Option<String>,
    #[serde(default)]
    pub torrent_path: Option<String>,
    #[serde(default)]
    pub files: Vec<TorrentFileInfo>,
    #[serde(default)]
    pub peers: usize,
    #[serde(default)]
    pub sources: usize,
    #[serde(default)]
    pub uploaded: u64,
    #[serde(default)]
    pub upload_speed: u64,
}
#[derive(Debug, Clone)]
pub struct TaskData {
    pub id: i64,
    pub url: String,
    pub dir: PathBuf,
    pub filename: String,
    pub filepath: PathBuf,
    pub part_path: PathBuf,
    pub cfg_path: PathBuf,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub progress: f64,
    pub speed: f64,
    pub status: String,
    pub error: Option<String>,
    pub kind: String,
    pub connections: usize,
    pub max_rate: u64,
    pub rate_explicit: bool,
    pub custom_headers: HashMap<String, String>,
    pub segments: Vec<Segment>,
    pub info_hash: Option<String>,
    pub ed2k_hash: Option<String>,
    pub torrent_path: Option<String>,
    pub files: Vec<TorrentFileInfo>,
    pub peers: usize,
    pub sources: usize,
    pub uploaded: u64,
    pub upload_speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfgFile {
    pub url: String,
    pub filename: String,
    pub filepath: String,
    pub total: Option<u64>,
    pub connections: usize,
    #[serde(default)]
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTask {
    pub id: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub filepath: String,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default)]
    pub max_rate: u64,
    #[serde(default)]
    pub rate_explicit: bool,
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub info_hash: Option<String>,
    #[serde(default)]
    pub ed2k_hash: Option<String>,
    #[serde(default)]
    pub torrent_path: Option<String>,
    #[serde(default)]
    pub files: Vec<TorrentFileInfo>,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub seed_after_complete: Option<bool>,
}

fn default_kind() -> String {
    "http".to_string()
}

fn default_connections() -> usize {
    8
}

impl TaskData {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        url: String,
        dir: impl AsRef<Path>,
        filename: Option<String>,
        connections: usize,
        max_rate: u64,
        rate_explicit: bool,
        custom_headers: HashMap<String, String>,
    ) -> Self {
        let dir = absolute_path(dir);
        let filename = filename
            .filter(|s| !s.trim().is_empty())
            .map(|s| sanitize_filename(&s))
            .unwrap_or_default();
        let filepath = if filename.is_empty() {
            PathBuf::new()
        } else {
            dir.join(&filename)
        };
        let part_path = if filepath.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(format!("{}.fdmpart", filepath.to_string_lossy()))
        };
        let cfg_path = if filepath.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(format!("{}.fdmcfg", filepath.to_string_lossy()))
        };
        Self {
            id,
            url,
            dir,
            filename,
            filepath,
            part_path,
            cfg_path,
            total: None,
            downloaded: 0,
            progress: 0.0,
            speed: 0.0,
            status: STATUS_QUEUED.to_string(),
            error: None,
            kind: "http".to_string(),
            connections: connections.clamp(1, 32),
            max_rate,
            rate_explicit,
            custom_headers,
            segments: Vec::new(),
            info_hash: None,
            ed2k_hash: None,
            torrent_path: None,
            files: Vec::new(),
            peers: 0,
            sources: 0,
            uploaded: 0,
            upload_speed: 0.0,
        }
    }

    pub fn from_saved(
        saved: SavedTask,
        default_dir: &Path,
        default_connections: usize,
        global_rate: u64,
    ) -> Self {
        let dir = if saved.dir.trim().is_empty() {
            default_dir.to_path_buf()
        } else {
            PathBuf::from(&saved.dir)
        };
        let connections = if saved.connections == 0 {
            default_connections
        } else {
            saved.connections
        };
        let rate = if saved.rate_explicit {
            saved.max_rate
        } else {
            global_rate
        };
        let saved_kind = if saved.kind.trim().is_empty() {
            "http".to_string()
        } else {
            saved.kind.clone()
        };
        let saved_info_hash = saved.info_hash.clone();
        let saved_ed2k_hash = saved.ed2k_hash.clone();
        let saved_torrent_path = saved.torrent_path.clone();
        let saved_files = saved.files.clone();
        let saved_output_dir = saved.output_dir.clone();
        let had_saved_filename = !saved.filename.trim().is_empty();
        let mut data = Self::new(
            saved.id,
            saved.url,
            dir,
            if had_saved_filename {
                Some(saved.filename)
            } else {
                None
            },
            connections,
            rate,
            saved.rate_explicit,
            saved.custom_headers,
        );
        if !saved.filepath.trim().is_empty() {
            data.filepath = PathBuf::from(&saved.filepath);
            data.part_path = PathBuf::from(format!("{}.fdmpart", data.filepath.to_string_lossy()));
            data.cfg_path = PathBuf::from(format!("{}.fdmcfg", data.filepath.to_string_lossy()));
            // 只有原本没有文件名时才从 filepath 反推；BT 任务保存的 filepath 是
            // 输出目录，直接反推会把任务名写成目录名（例如 “Downloads”）。
            if !had_saved_filename {
                if let Some(name) = data.filepath.file_name() {
                    data.filename = name.to_string_lossy().into_owned();
                }
            }
        }
        data.total = saved.total;
        data.downloaded = saved.downloaded;
        data.progress = saved.progress;
        data.status = if is_active_status(&saved.status)
            || saved.status == STATUS_QUEUED
            || saved.status.is_empty()
        {
            STATUS_PAUSED.to_string()
        } else {
            saved.status
        };
        data.error = saved.error;
        data.kind = saved_kind;
        data.info_hash = saved_info_hash;
        data.ed2k_hash = saved_ed2k_hash;
        data.torrent_path = saved_torrent_path;
        data.files = saved_files;
        // BT 磁力任务用 dn 修正任务名：既修复旧数据里被写成目录名的记录，
        // 也保证重启后任务列表仍显示真实文件名。
        if data.kind == "bt" {
            if let Some(magnet) = crate::parse_magnet(&data.url) {
                if let Some(name) = magnet.name {
                    data.filename = sanitize_filename(&name);
                }
            }
        }
        if let Some(out) = saved_output_dir.filter(|s| !s.trim().is_empty()) {
            data.dir = PathBuf::from(out);
            if !data.filename.is_empty() {
                data.filepath = data.dir.join(&data.filename);
            }
        }
        if data.kind == "http" {
            let _ = data.load_cfg();
        }
        data.refresh_progress();
        data
    }

    pub fn setup_paths(&mut self) {
        let name = if self.filename.is_empty() {
            filename_from_url(&self.url).unwrap_or_else(|| "download.bin".to_string())
        } else {
            sanitize_filename(&self.filename)
        };
        self.filename = name;
        self.filepath = self.dir.join(&self.filename);
        self.part_path = PathBuf::from(format!("{}.fdmpart", self.filepath.to_string_lossy()));
        self.cfg_path = PathBuf::from(format!("{}.fdmcfg", self.filepath.to_string_lossy()));
    }

    pub fn refresh_progress(&mut self) {
        // 已完成任务以状态为准，固定为 100%。历史记录或分块元数据可能
        // 残留 98% 左右的数据，不应让进度条与“完成”状态互相矛盾。
        if self.status == STATUS_DONE {
            self.progress = 1.0;
            if let Some(total) = self.total {
                self.downloaded = total;
            }
            return;
        }
        if !self.segments.is_empty() {
            if let Some(total) = self.total {
                self.downloaded = self.segments.iter().map(Segment::downloaded).sum();
                self.progress = if total > 0 {
                    round4(self.downloaded as f64 / total as f64)
                } else {
                    0.0
                };
            }
        } else if let Some(total) = self.total {
            if total > 0 {
                self.progress = round4(self.downloaded as f64 / total as f64);
            }
        }
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        let filename = if !self.filename.is_empty() {
            self.filename.clone()
        } else {
            self.filepath
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download".to_string())
        };
        TaskSnapshot {
            id: self.id,
            url: self.url.clone(),
            filename,
            dir: path_to_string(&self.dir),
            filepath: path_to_string(&self.filepath),
            total: self.total,
            downloaded: self.downloaded,
            progress: self.progress,
            speed: if self.status == STATUS_DOWNLOADING {
                self.speed.max(0.0) as u64
            } else {
                0
            },
            status: self.status.clone(),
            error: self.error.clone(),
            kind: self.kind.clone(),
            connections: self.connection_count(),
            segments: self.segments.clone(),
            info_hash: self.info_hash.clone(),
            ed2k_hash: self.ed2k_hash.clone(),
            torrent_path: self.torrent_path.clone(),
            files: self.files.clone(),
            peers: self.peers,
            sources: self.sources,
            uploaded: self.uploaded,
            upload_speed: if matches!(self.status.as_str(), STATUS_DOWNLOADING | STATUS_SEEDING) {
                self.upload_speed.max(0.0) as u64
            } else {
                0
            },
        }
    }

    pub fn segments_complete(&self) -> bool {
        !self.segments.is_empty() && self.segments.iter().all(Segment::complete)
    }

    /// 实际参与本次下载的连接数，而不是分片数；工作窃取产生更多分片时不应放大 UI 连接数。
    pub fn connection_count(&self) -> usize {
        let workers: HashSet<usize> = self
            .segments
            .iter()
            .filter_map(|segment| segment.worker)
            .collect();
        if !workers.is_empty() {
            workers.len().min(self.connections.max(1))
        } else if self.segments.is_empty() {
            1
        } else {
            self.connections.max(1).min(self.segments.len().max(1))
        }
    }

    /// 供后台阻塞线程写盘使用的 .fdmcfg 快照。
    pub fn cfg_payload(&self) -> Option<(PathBuf, Vec<u8>)> {
        if self.kind != "http" || self.cfg_path.as_os_str().is_empty() || self.segments.is_empty() {
            return None;
        }
        let cfg = CfgFile {
            url: self.url.clone(),
            filename: self.filename.clone(),
            filepath: path_to_string(&self.filepath),
            total: self.total,
            connections: self.connections,
            segments: self.segments.clone(),
        };
        serde_json::to_vec_pretty(&cfg)
            .ok()
            .map(|bytes| (self.cfg_path.clone(), bytes))
    }

    pub fn save_cfg(&self) {
        // BT/ED2K 的进度由各自引擎持久化，这里再写 100KB+ 的 .fdmcfg 既冗余又会残留。
        if self.kind != "http" {
            return;
        }
        if self.cfg_path.as_os_str().is_empty() || self.segments.is_empty() {
            return;
        }
        let cfg = CfgFile {
            url: self.url.clone(),
            filename: self.filename.clone(),
            filepath: path_to_string(&self.filepath),
            total: self.total,
            connections: self.connections,
            segments: self.segments.clone(),
        };
        if let Err(err) = paths::write_json_pretty(&self.cfg_path, &cfg) {
            tracing::warn!(error = %err, path = %self.cfg_path.display(), "保存断点配置失败");
        }
    }

    pub fn load_cfg(&mut self) -> bool {
        if self.cfg_path.as_os_str().is_empty() || !self.cfg_path.exists() {
            return false;
        }
        let Ok(cfg) = paths::read_json::<CfgFile>(&self.cfg_path) else {
            return false;
        };
        self.total = cfg.total;
        if !cfg.filename.is_empty() {
            self.filename = cfg.filename;
        }
        if !cfg.filepath.is_empty() {
            self.filepath = PathBuf::from(&cfg.filepath);
            self.part_path = PathBuf::from(format!("{}.fdmpart", self.filepath.to_string_lossy()));
            self.cfg_path = PathBuf::from(format!("{}.fdmcfg", self.filepath.to_string_lossy()));
        }
        self.segments = cfg.segments;
        for segment in &mut self.segments {
            segment.worker = None;
        }
        self.downloaded = self.segments.iter().map(Segment::downloaded).sum();
        self.refresh_progress();
        true
    }

    pub fn saved(&self) -> SavedTask {
        SavedTask {
            id: self.id,
            url: self.url.clone(),
            dir: path_to_string(&self.dir),
            filename: self.filename.clone(),
            filepath: path_to_string(&self.filepath),
            total: self.total,
            downloaded: self.downloaded,
            progress: self.progress,
            status: self.status.clone(),
            error: self.error.clone(),
            connections: self.connections,
            max_rate: self.max_rate,
            rate_explicit: self.rate_explicit,
            custom_headers: self.custom_headers.clone(),
            kind: self.kind.clone(),
            info_hash: self.info_hash.clone(),
            ed2k_hash: self.ed2k_hash.clone(),
            torrent_path: self.torrent_path.clone(),
            files: self.files.clone(),
            output_dir: Some(path_to_string(&self.dir)),
            seed_after_complete: None,
        }
    }
}

fn round4(value: f64) -> f64 {
    (value * 10000.0).round() / 10000.0
}

pub fn sanitize_filename(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    let result = result.trim().trim_matches('.').trim().to_string();
    if result.is_empty() {
        "download".to_string()
    } else {
        result
    }
}

pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    if let Some(pos) = lower.find("filename*=") {
        let rest = &value[pos + "filename*=".len()..];
        let raw = rest
            .split(';')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        let raw = raw
            .strip_prefix("UTF-8''")
            .or_else(|| raw.strip_prefix("utf-8''"))
            .unwrap_or(raw);
        let decoded = percent_decode_str(raw).decode_utf8_lossy();
        return Some(sanitize_filename(&decoded));
    }
    if let Some(pos) = lower.find("filename=") {
        let rest = &value[pos + "filename=".len()..];
        let raw = rest
            .split(';')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if !raw.is_empty() {
            let decoded = percent_decode_str(raw).decode_utf8_lossy();
            return Some(sanitize_filename(&decoded));
        }
    }
    None
}

pub fn filename_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let path = parsed.path().trim_end_matches('/');
    let base = path.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        return None;
    }
    let decoded = percent_decode_str(base).decode_utf8_lossy();
    Some(sanitize_filename(&decoded))
}

pub fn filename_from_headers_or_url(url: &str, content_disposition: Option<&str>) -> String {
    if let Some(cd) = content_disposition {
        if let Some(name) = filename_from_content_disposition(cd) {
            return name;
        }
    }
    filename_from_url(url).unwrap_or_else(|| "download.bin".to_string())
}

/// 规范化 URL，用于重复下载判断。
/// 统一 scheme/host 大小写、去掉默认端口与片段，并剔除签名/时效类查询参数，
/// 避免同一文件每次生成的临时签名链接被误判为不同链接。
/// 解析失败时退回原始 trim 文本。
pub fn canonical_url(raw: &str) -> String {
    let raw = raw.trim();
    let Ok(mut parsed) = Url::parse(raw) else {
        return raw.to_string();
    };
    parsed.set_fragment(None);
    let scheme = parsed.scheme().to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        "ftp" => Some(21),
        _ => None,
    };
    if parsed.port().is_some() && parsed.port() == default_port {
        let _ = parsed.set_port(None);
    }

    // 识别 Azure Blob SAS / GitHub Release 资产等带短参数名的签名链接。
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let azure_signed = host.ends_with("githubusercontent.com")
        || host.ends_with("blob.core.windows.net")
        || parsed.query_pairs().any(|(k, _)| {
            k.eq_ignore_ascii_case("sig")
                || k.eq_ignore_ascii_case("skoid")
                || k.eq_ignore_ascii_case("jwt")
        });

    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !is_volatile_query_key(k, azure_signed))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        parsed.set_query(None);
    } else {
        // 参数顺序不影响资源身份，排序后保证同一链接的不同书写顺序得到相同结果。
        pairs.sort_by(|a, b| {
            a.0.to_ascii_lowercase()
                .cmp(&b.0.to_ascii_lowercase())
                .then_with(|| a.1.cmp(&b.1))
        });
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            ser.append_pair(k, v);
        }
        parsed.set_query(Some(&ser.finish()));
    }
    parsed.to_string()
}

/// 是否为签名、时效或缓存控制类的易变查询参数。
fn is_volatile_query_key(key: &str, azure_signed: bool) -> bool {
    let k = key.to_ascii_lowercase();
    if k == "jwt"
        || k == "sig"
        || k == "signature"
        || k == "token"
        || k == "access_token"
        || k == "expires"
        || k == "expiry"
        || k == "expire"
        || k == "auth_key"
        || k == "policy"
        || k == "key-pair-id"
        || k.starts_with("x-amz-")
        || k.starts_with("x-goog-")
        || k.starts_with("response-content-")
        || k.starts_with("response-cache-")
        || k.starts_with("response-expires")
    {
        return true;
    }
    // Azure SAS 的短参数名过于通用，只在确认是签名链接时剔除。
    azure_signed
        && matches!(
            k.as_str(),
            "sp" | "st"
                | "se"
                | "sv"
                | "sr"
                | "spr"
                | "skt"
                | "ske"
                | "sks"
                | "skoid"
                | "sktid"
                | "skv"
                | "rscd"
                | "rsct"
        )
}

/// 解析最终要保存的文件名：优先用户指定，其次从 URL 推导，最后 download.bin。
pub fn resolve_filename(url: &str, requested: Option<&str>) -> String {
    requested
        .map(sanitize_filename)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| filename_from_url(url))
        .unwrap_or_else(|| "download.bin".to_string())
}

/// 生成带序号的文件名：file.zip -> file (1).zip
pub fn numbered_filename(filename: &str, number: u32) -> String {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| filename.to_string());
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    format!("{stem} ({number}){ext}")
}

pub fn task_cfg_path(filepath: &Path) -> PathBuf {
    PathBuf::from(format!("{}.fdmcfg", filepath.to_string_lossy()))
}

pub fn existing_parent(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        path.parent().filter(|p| p.exists()).map(Path::to_path_buf)
    }
}

pub fn ensure_task_dir(data: &TaskData) -> Result<()> {
    paths::ensure_dir(&data.dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_parsing() {
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.zip"
            )
            .as_deref(),
            Some("测试.zip")
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"a/b.txt\"").as_deref(),
            Some("a_b.txt")
        );
        assert_eq!(
            filename_from_url("https://example.com/a/b%20c.bin").as_deref(),
            Some("b c.bin")
        );
    }

    #[test]
    fn duplicate_helpers() {
        assert_eq!(
            canonical_url("HTTPS://Example.COM:443/a?b=1#x"),
            "https://example.com/a?b=1"
        );
        assert_eq!(
            canonical_url("http://example.com:80/a"),
            "http://example.com/a"
        );
        assert_eq!(
            canonical_url("http://example.com:8080/a"),
            "http://example.com:8080/a"
        );
        assert_eq!(
            resolve_filename("https://example.com/a/b.zip", None),
            "b.zip"
        );
        assert_eq!(
            resolve_filename("https://example.com/a/", Some("my.bin")),
            "my.bin"
        );
        assert_eq!(numbered_filename("file.zip", 1), "file (1).zip");
        assert_eq!(numbered_filename("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered_filename("README", 3), "README (3)");
    }

    #[test]
    fn canonical_url_ignores_signed_query_params() {
        let a = "https://release-assets.githubusercontent.com/github-production-release-asset/1/a.tzst?sp=r&sv=2018-11-09&sr=b&sig=AAA&jwt=BBB&se=2026-10-22T14%3A32%3A57Z&rscd=attachment%3B+filename%3Da.tzst";
        let b = "https://release-assets.githubusercontent.com/github-production-release-asset/1/a.tzst?sp=r&sv=2018-11-09&sr=b&sig=CCC&jwt=DDD&se=2026-10-23T10%3A00%3A00Z&rscd=attachment%3B+filename%3Da.tzst";
        assert_eq!(canonical_url(a), canonical_url(b));
        assert_eq!(
            canonical_url(a),
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/a.tzst"
        );
        assert_ne!(
            canonical_url(a),
            canonical_url("https://release-assets.githubusercontent.com/github-production-release-asset/2/a.tzst?sp=r&sig=AAA")
        );
    }

    #[test]
    fn canonical_url_sorts_query_params() {
        assert_eq!(
            canonical_url("https://example.com/a?b=1&a=2"),
            canonical_url("https://example.com/a?a=2&b=1")
        );
    }
}
