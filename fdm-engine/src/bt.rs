use std::collections::{HashMap, HashSet};
use std::net::{Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use fdm_core::{paths, SettingsStore, TorrentFileInfo};
use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions, Session,
    SessionOptions, SessionPersistenceConfig, TorrentStatsState,
};
use tokio::sync::RwLock;
use url::Url;

const METADATA_TIMEOUT: Duration = Duration::from_secs(90);

/// 内置公共 tracker。magnet 链接经常不带 `tr=` 参数，只靠 DHT/PEX 找 peer
/// 首包会很慢；这里补一组长期存活的公共 tracker（与主流 BT 客户端内置列表一致）。
const PUBLIC_TRACKERS: &[&str] = &[
    // UDP tracker：普通网络下开销最小。
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.skynetcloud.site:6969/announce",
    "udp://tracker.qu.ax:6969/announce",
    "udp://tracker.corpscorp.online:80/announce",
    "udp://tracker.bittor.pw:1337/announce",
    "udp://tracker.ducks.party:1984/announce",
    "udp://tracker-udp.gbitt.info:80/announce",
    "udp://tracker.nyaa.vc:6969/announce",
    "udp://retracker01-msk-virt.corbina.net:80/announce",
    "udp://tracker.gmi.gd:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://tracker.peerfect.org:6969/announce",
    "udp://tracker.opentrackr.com:6969/announce",
    // HTTP(S) tracker：在代理只放行 TCP、屏蔽 UDP 的网络里仍然可用。
    "http://tracker.opentrackr.org:1337/announce",
    "http://tracker.qu.ax:6969/announce",
    "http://tracker.dler.org:6969/announce",
    "http://tracker2.dler.org:80/announce",
    "http://tracker.dler.com:6969/announce",
    "http://tracker.renfei.net:8080/announce",
    "http://open.tracker.cl:1337/announce",
    "http://tracker.bittor.pw:1337/announce",
    "http://tracker.mywaifu.best:6969/announce",
    "https://tracker.zhuqiy.com:443/announce",
    "https://tracker.foreverpirates.co:443/announce",
    "https://open.ftorrent.com:443/announce",
    "https://tracker.pmman.tech:443/announce",
    "https://tracker.nekomi.cn:443/announce",
    "https://tr.nyacat.pw:443/announce",
    "https://ht.therarbg.to:443/announce",
];

/// DHT 引导节点。与迅雷 DownloadSDK 内置的 OriginNodes 完全一致。
const DHT_BOOTSTRAP: &[&str] = &[
    "dht.transmissionbt.com:6881",
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.aelitis.com:6881",
    "dht.libtorrent.org:25401",
];

/// BT 任务的来源。
#[derive(Debug, Clone)]
pub enum BtSource {
    Magnet(String),
    TorrentBytes(Vec<u8>),
}

/// BT 引擎返回的种子元数据。
#[derive(Debug, Clone)]
pub struct BtTorrentInfo {
    pub info_hash: String,
    pub name: String,
    pub total: u64,
    pub files: Vec<TorrentFileInfo>,
    pub output_dir: PathBuf,
}

/// BT 任务当前状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtState {
    Initializing,
    Live,
    Paused,
    Finished,
    Error,
}

/// BT 任务的一次进度快照。
#[derive(Debug, Clone, Default)]
pub struct BtStats {
    pub state: Option<BtState>,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub peers: usize,
    pub files: Vec<u64>,
    pub error: Option<String>,
}

type TorrentHandle = Arc<librqbit::ManagedTorrent>;

/// 进程内 BT 引擎。所有 BT 任务共享一个 librqbit Session。
pub struct BtEngine {
    session: Arc<Session>,
    handles: RwLock<HashMap<i64, TorrentHandle>>,
    _settings: Arc<SettingsStore>,
    peer_limit: usize,
}

impl BtEngine {
    pub async fn new(data_dir: &Path, settings: Arc<SettingsStore>) -> Result<Arc<Self>> {
        let bt = settings.get().bt;
        if !bt.enabled {
            return Err(anyhow!("BT 已禁用"));
        }
        let root = data_dir.join("bt");
        paths::ensure_dir(&root)?;
        paths::ensure_dir(&root.join("session"))?;
        paths::ensure_dir(&root.join("dht"))?;

        let ratelimits = LimitsConfig {
            download_bps: None,
            upload_bps: None,
        };
        // 双栈监听 + UPnP 端口映射，尽量拿到 IPv6 peer 与可回连端口（更多 peer = 更快）。
        let listen = ListenerOptions {
            listen_addr: SocketAddr::from((Ipv6Addr::UNSPECIFIED, bt.listen_port)),
            enable_upnp_port_forwarding: true,
            announce_port: Some(bt.listen_port),
            ..Default::default()
        };
        let dht = if bt.dht_enabled {
            Some(DhtSessionConfig {
                bootstrap_addrs: Some(DHT_BOOTSTRAP.iter().map(|s| s.to_string()).collect()),
                port: None,
                persistence: Some(DhtPersistenceConfig {
                    config_filename: Some(root.join("dht").join("dht.json")),
                    ..Default::default()
                }),
            })
        } else {
            None
        };
        let global_trackers: HashSet<Url> = PUBLIC_TRACKERS
            .iter()
            .filter_map(|t| Url::parse(t).ok())
            .collect();
        let opts = SessionOptions {
            dht,
            disable_trackers: !bt.tracker_enabled,
            trackers: global_trackers,
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(root.join("session")),
            }),
            listen: Some(listen),
            peer_limit: Some(bt.max_peers as usize),
            ratelimits,
            client_name_and_version: Some(format!("FDM {}", env!("CARGO_PKG_VERSION"))),
            ..Default::default()
        };
        let session = Session::new_with_opts(root.join("downloads"), opts)
            .await
            .context("初始化 BT Session 失败")?;
        // librqbit 会按持久化元数据自动恢复 torrent（含未暂停的旧会话）。
        // FDM 的“暂停”状态由 TaskManager 管理，启动时先统一暂停，再由
        // reconcile_p2p 决定哪些任务需要恢复，避免后台静默下载。
        let restored = std::cell::RefCell::new(Vec::new());
        session.with_torrents(|iter| {
            for (_, handle) in iter {
                restored.borrow_mut().push(handle.clone());
            }
        });
        for handle in restored.into_inner() {
            let _ = session.pause(&handle).await;
        }
        Ok(Arc::new(Self {
            session,
            handles: RwLock::new(HashMap::new()),
            _settings: settings,
            peer_limit: bt.max_peers as usize,
        }))
    }

    pub fn listen_port(&self) -> u16 {
        self.session
            .listen_addr()
            .map(|addr| addr.port())
            .unwrap_or(0)
    }

    pub fn dht_nodes(&self) -> Option<u64> {
        self.session
            .get_dht()
            .map(|dht| dht.stats().routing_table_size as u64)
    }

    /// 仅解析 .torrent 字节，不加入下载队列。
    pub async fn inspect_torrent(&self, bytes: Vec<u8>) -> Result<BtTorrentInfo> {
        let response = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(Bytes::from(bytes)),
                Some(AddTorrentOptions {
                    list_only: true,
                    ..Default::default()
                }),
            )
            .await
            .context("解析种子失败")?;
        match response {
            AddTorrentResponse::ListOnly(list) => {
                let name = list
                    .info
                    .name()
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|| "torrent".to_string());
                let files = collect_files(&list.info);
                let total = files.iter().map(|f| f.length).sum();
                Ok(BtTorrentInfo {
                    info_hash: list.info_hash.as_string(),
                    name,
                    total,
                    files,
                    output_dir: list.output_folder,
                })
            }
            _ => Err(anyhow!("解析种子返回了意外的结果")),
        }
    }

    /// 添加种子并等待元数据就绪，返回名称、文件列表和保存目录。
    pub async fn add(
        &self,
        id: i64,
        source: BtSource,
        dir: &Path,
        paused: bool,
        initial_peers: Option<Vec<SocketAddr>>,
    ) -> Result<BtTorrentInfo> {
        paths::ensure_dir(dir)?;
        let add = match source {
            BtSource::Magnet(value) => AddTorrent::from_url(value),
            BtSource::TorrentBytes(bytes) => AddTorrent::from_bytes(Bytes::from(bytes)),
        };
        let response = self
            .session
            .add_torrent(
                add,
                Some(AddTorrentOptions {
                    paused,
                    overwrite: true,
                    output_folder: Some(dir.to_string_lossy().into_owned()),
                    peer_limit: Some(self.peer_limit),
                    initial_peers,
                    trackers: Some(PUBLIC_TRACKERS.iter().map(|t| t.to_string()).collect()),
                    force_tracker_interval: Some(Duration::from_secs(60)),
                    ..Default::default()
                }),
            )
            .await
            .context("添加 BT 任务失败")?;
        let handle = match response {
            AddTorrentResponse::Added(_, handle)
            | AddTorrentResponse::AlreadyManaged(_, handle) => handle,
            AddTorrentResponse::ListOnly(_) => return Err(anyhow!("种子未进入下载队列")),
        };

        tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized())
            .await
            .map_err(|_| anyhow!("获取种子元数据超时（90 秒）"))?
            .context("初始化 BT 任务失败")?;

        let info = handle
            .with_metadata(|meta| {
                let name = meta
                    .info
                    .name()
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|| "torrent".to_string());
                let files = collect_files(&meta.info);
                let total = files.iter().map(|f| f.length).sum();
                BtTorrentInfo {
                    info_hash: handle.info_hash().as_string(),
                    name,
                    total,
                    files,
                    output_dir: handle.output_folder().to_path_buf(),
                }
            })
            .context("读取种子元数据失败")?;

        self.handles.write().await.insert(id, handle);
        Ok(info)
    }

    pub async fn stats(&self, id: i64) -> Option<BtStats> {
        let handle = self.handles.read().await.get(&id).cloned()?;
        let stats = handle.stats();
        let state = match stats.state {
            TorrentStatsState::Initializing { paused } => {
                if paused {
                    BtState::Paused
                } else {
                    BtState::Initializing
                }
            }
            TorrentStatsState::Live => {
                if stats.finished {
                    BtState::Finished
                } else {
                    BtState::Live
                }
            }
            TorrentStatsState::Paused => {
                // 启动时会先统一暂停已持久化的 torrent；已完成的任务仍应
                // 保持“完成/做种”语义，不能被误判成用户暂停。
                if stats.finished {
                    BtState::Finished
                } else {
                    BtState::Paused
                }
            }
            TorrentStatsState::Error => BtState::Error,
        };
        let (download_speed, upload_speed, peers) = match stats.live.as_ref() {
            Some(live) => (
                live.download_speed.as_bytes(),
                live.upload_speed.as_bytes(),
                (live.snapshot.peer_stats.live + live.snapshot.peer_stats.connecting) as usize,
            ),
            None => (0, 0, 0),
        };
        Some(BtStats {
            state: Some(state),
            progress_bytes: stats.progress_bytes,
            total_bytes: stats.total_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            download_speed,
            upload_speed,
            peers,
            files: stats.file_progress,
            error: stats.error,
        })
    }

    pub async fn pause(&self, id: i64) -> bool {
        let Some(handle) = self.handles.read().await.get(&id).cloned() else {
            return false;
        };
        self.session.pause(&handle).await.is_ok()
    }

    pub async fn resume(&self, id: i64) -> bool {
        let Some(handle) = self.handles.read().await.get(&id).cloned() else {
            return false;
        };
        self.session.unpause(&handle).await.is_ok()
    }

    pub async fn remove(&self, id: i64, delete_files: bool) -> bool {
        let handle = self.handles.write().await.remove(&id);
        let Some(handle) = handle else {
            return false;
        };
        let result = self
            .session
            .delete(handle.info_hash().into(), delete_files)
            .await;
        result.is_ok()
    }

    /// 重启后把已持久化的 torrent 重新挂到任务 id 上。
    pub async fn reattach(&self, id: i64, info_hash: &str) -> bool {
        let info_hash = info_hash.to_ascii_lowercase();
        let found = std::cell::RefCell::new(None::<TorrentHandle>);
        self.session.with_torrents(|iter| {
            for (_, handle) in iter {
                if handle.info_hash().as_string() == info_hash {
                    *found.borrow_mut() = Some(handle.clone());
                    break;
                }
            }
        });
        if let Some(handle) = found.into_inner() {
            self.handles.write().await.insert(id, handle);
            true
        } else {
            false
        }
    }

    pub async fn set_global_rate(&self, kb_per_sec: u64) {
        let bps = NonZeroU32::new(kb_per_sec.saturating_mul(1024).min(u32::MAX as u64) as u32);
        self.session.ratelimits.set_download_bps(bps);
    }

    pub async fn shutdown(&self) {
        self.session.stop().await;
    }
}

fn collect_files(
    info: &librqbit::ValidatedTorrentMetaV1Info<librqbit::ByteBufOwned>,
) -> Vec<TorrentFileInfo> {
    info.iter_file_details()
        .enumerate()
        .map(|(index, file)| TorrentFileInfo {
            index,
            path: file.filename.to_pathbuf().to_string_lossy().into_owned(),
            length: file.len,
            progress: 0,
            selected: true,
        })
        .collect()
}
