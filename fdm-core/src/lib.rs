pub mod error;
pub mod github_proxy;
pub mod model;
pub mod paths;
pub mod protocol;
pub mod settings;

pub use error::{CoreError, Result};
pub use github_proxy::{
    github_proxy_candidates, has_sensitive_headers, is_eligible_github_url, normalize_prefix,
    GITHUB_PROXY_PRESETS,
};
pub use model::*;
pub use paths::{app_dir, data_dir, resolve_data_dir};
pub use protocol::{detect_kind, parse_ed2k_link, parse_magnet, Ed2kLink, MagnetInfo};
pub use settings::{BtSettings, Ed2kSettings, Settings, SettingsStore};
