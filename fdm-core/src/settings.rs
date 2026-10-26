use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CoreError, Result};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BtSettings {
    pub enabled: bool,
    pub listen_port: u16,
    pub dht_enabled: bool,
    pub tracker_enabled: bool,
    pub max_peers: u32,
    pub seed_after_complete: bool,
}

impl Default for BtSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_port: 25100,
            dht_enabled: true,
            tracker_enabled: true,
            max_peers: 200,
            seed_after_complete: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ed2kSettings {
    pub enabled: bool,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub ec_port: u16,
    pub api_port: u16,
    pub server_met_url: String,
    pub nodes_dat_url: String,
    pub share_completed: bool,
    pub download_limit_kb: u64,
}

impl Default for Ed2kSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            tcp_port: 4662,
            udp_port: 4672,
            ec_port: 14712,
            api_port: 14713,
            server_met_url: "https://upd.emule-security.org/server.met".to_string(),
            nodes_dat_url: "https://upd.emule-security.org/nodes.dat".to_string(),
            share_completed: false,
            download_limit_kb: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub default_dir: String,
    pub default_connections: u32,
    pub max_concurrent: u32,
    #[serde(rename = "maxRateKB")]
    pub max_rate_kb: u64,
    pub bt: BtSettings,
    pub ed2k: Ed2kSettings,
    pub auto_download: bool,
    /// 监视剪贴板中的 ED2K / BT 链接，复制后自动弹出新建任务窗口
    pub monitor_clipboard: bool,
    /// 删除任务时是否同时删除已下载的本地文件
    pub delete_task_files: bool,
    /// 重复下载链接处理：0=每次询问，1=带序号另存，2=覆盖，3=完成则显示完成对话框/否则继续
    pub duplicate_action: u8,
    /// 是否记住上面的重复下载处理方式
    pub remember_duplicate_action: bool,
    /// 同名文件冲突处理：0=每次询问，1=使用带序号的文件名，2=覆盖同名文件
    pub file_conflict_action: u8,
    /// 是否记住同名文件冲突的处理方式
    pub remember_file_conflict_action: bool,
    pub done_open_folder: bool,
    pub done_open_file: bool,
    pub done_close_progress: bool,
    pub auto_launch: bool,
    pub auto_launch_default_migrated: bool,
    /// 注册 magnet / ed2k / .torrent 文件关联（Windows 当前用户）
    pub file_associations: bool,
    /// 首次安装时是否已经展示过浏览器扩展安装引导
    pub extension_guide_shown: bool,
    pub lang: String,
    pub proxy_enabled: bool,
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_user: String,
    pub proxy_pass: String,
    /// 是否启用 GitHub 下载加速
    pub github_proxy_enabled: bool,
    /// 自定义/首选 GitHub 代理前缀；空表示自动使用内置预设
    pub github_proxy_url: String,
    /// 所有代理失败后是否回退原始直连
    pub github_proxy_fallback_direct: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_dir: system_download_dir().to_string_lossy().into_owned(),
            default_connections: 8,
            max_concurrent: 5,
            max_rate_kb: 0,
            bt: BtSettings::default(),
            ed2k: Ed2kSettings::default(),
            auto_download: true,
            monitor_clipboard: true,
            delete_task_files: true,
            duplicate_action: 3,
            remember_duplicate_action: false,
            file_conflict_action: 0,
            remember_file_conflict_action: false,
            done_open_folder: true,
            done_open_file: false,
            done_close_progress: true,
            auto_launch: true,
            auto_launch_default_migrated: true,
            file_associations: true,
            extension_guide_shown: false,
            lang: "zh-CN".to_string(),
            proxy_enabled: false,
            proxy_type: "http".to_string(),
            proxy_host: String::new(),
            proxy_port: String::new(),
            proxy_user: String::new(),
            proxy_pass: String::new(),
            github_proxy_enabled: true,
            github_proxy_url: String::new(),
            github_proxy_fallback_direct: true,
            extra: BTreeMap::new(),
        }
    }
}

pub struct SettingsStore {
    path: PathBuf,
    settings: RwLock<Settings>,
}

impl SettingsStore {
    pub fn load(data_dir: &Path) -> Result<Self> {
        paths::ensure_dir(data_dir)?;
        let path = data_dir.join("settings.json");
        let mut settings = Settings::default();
        let mut migrated = false;

        if path.exists() {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            {
                Some(value) => {
                    let had_migration_flag = value
                        .get("autoLaunchDefaultMigrated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // 旧版本没有该字段：视为已有用户，升级后不再自动弹首次安装引导。
                    let had_extension_guide_flag = value.get("extensionGuideShown").is_some();
                    if let Ok(parsed) = serde_json::from_value::<Settings>(value) {
                        settings = parsed;
                    }
                    if !had_migration_flag {
                        settings.auto_launch = true;
                        settings.auto_launch_default_migrated = true;
                        migrated = true;
                    }
                    if !had_extension_guide_flag {
                        settings.extension_guide_shown = true;
                        migrated = true;
                    }
                }
                None => tracing::warn!(path = %path.display(), "设置文件解析失败，使用默认设置"),
            }
        } else {
            migrated = true;
        }

        let store = Self {
            path,
            settings: RwLock::new(settings),
        };
        if migrated {
            store.save()?;
        }
        Ok(store)
    }

    pub fn get(&self) -> Settings {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn get_value(&self) -> Value {
        serde_json::to_value(self.get()).unwrap_or_else(|_| Value::Object(Default::default()))
    }

    pub fn update_patch(&self, patch: &Value) -> Result<Settings> {
        let mut current = serde_json::to_value(self.get())?;
        if let (Some(current_obj), Some(patch_obj)) = (current.as_object_mut(), patch.as_object()) {
            for (key, value) in patch_obj {
                current_obj.insert(key.clone(), value.clone());
            }
        }
        let updated: Settings = serde_json::from_value(current)?;
        *self
            .settings
            .write()
            .map_err(|_| CoreError::Other("设置锁损坏".into()))? = updated.clone();
        self.save()?;
        Ok(updated)
    }

    pub fn save(&self) -> Result<()> {
        let snapshot = self.get();
        paths::write_json_pretty(&self.path, &snapshot)
    }
}

pub fn system_download_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(path) = windows_download_dir() {
            if path.is_dir() {
                return path;
            }
        }
    }
    let fallback = paths::home_dir().join("Downloads");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

#[cfg(windows)]
fn windows_download_dir() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders")
        .ok()?;
    let raw: String = key
        .get_value("{374DE290-123F-4565-9164-39C4925E467B}")
        .ok()?;
    let expanded = expand_env_vars(&raw);
    let path = PathBuf::from(expanded);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

pub fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(offset) = chars[i + 1..].iter().position(|c| *c == '%') {
                let end = i + 1 + offset;
                let name: String = chars[i + 1..end].iter().collect();
                if !name.is_empty() {
                    if let Ok(value) = std::env::var(&name) {
                        out.push_str(&value);
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_auto_launch_default_is_migrated_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, r#"{"autoLaunch":false,"defaultDir":"D:/Downloads"}"#).unwrap();
        let store = SettingsStore::load(temp.path()).unwrap();
        let settings = store.get();
        assert!(settings.auto_launch);
        assert!(settings.auto_launch_default_migrated);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved
                .get("autoLaunchDefaultMigrated")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn defaults_keep_legacy_camel_case() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert!(value.get("defaultDir").is_some());
        assert!(value.get("maxRateKB").is_some());
        assert!(value.get("autoLaunchDefaultMigrated").is_some());
        assert!(value.get("fileAssociations").is_some());
        assert!(value.get("extensionGuideShown").is_some());
        assert!(value.get("deleteTaskFiles").is_some());
        assert!(value.get("githubProxyEnabled").is_some());
        assert!(value.get("githubProxyUrl").is_some());
        assert!(value.get("githubProxyFallbackDirect").is_some());
    }

    #[test]
    fn delete_task_files_defaults_on_and_can_be_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");

        // 旧配置缺少该字段时按默认开启处理。
        std::fs::write(&path, r#"{"defaultDir":"D:/Downloads"}"#).unwrap();
        let store = SettingsStore::load(temp.path()).unwrap();
        assert!(store.get().delete_task_files);

        // 显式关闭后应能保存并重新读取。
        store
            .update_patch(&serde_json::json!({ "deleteTaskFiles": false }))
            .unwrap();
        assert!(!store.get().delete_task_files);
        let reloaded = SettingsStore::load(temp.path()).unwrap();
        assert!(!reloaded.get().delete_task_files);
    }

    #[test]
    fn github_proxy_defaults_are_backward_compatible() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, r#"{"defaultDir":"D:/Downloads"}"#).unwrap();
        let store = SettingsStore::load(temp.path()).unwrap();
        let settings = store.get();
        assert!(settings.github_proxy_enabled);
        assert!(settings.github_proxy_url.is_empty());
        assert!(settings.github_proxy_fallback_direct);

        store
            .update_patch(&serde_json::json!({
                "githubProxyEnabled": true,
                "githubProxyUrl": "https://ghfast.top/",
                "githubProxyFallbackDirect": false
            }))
            .unwrap();
        let reloaded = SettingsStore::load(temp.path()).unwrap();
        assert!(reloaded.get().github_proxy_enabled);
        assert_eq!(reloaded.get().github_proxy_url, "https://ghfast.top/");
        assert!(!reloaded.get().github_proxy_fallback_direct);
    }

    #[test]
    fn fresh_install_keeps_extension_guide_pending() {
        let temp = tempfile::tempdir().unwrap();
        let store = SettingsStore::load(temp.path()).unwrap();
        assert!(!store.get().extension_guide_shown);
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(temp.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved.get("extensionGuideShown").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn existing_settings_are_treated_as_extension_guide_shown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, r#"{"defaultDir":"D:/Downloads"}"#).unwrap();
        let store = SettingsStore::load(temp.path()).unwrap();
        assert!(store.get().extension_guide_shown);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.get("extensionGuideShown").and_then(Value::as_bool),
            Some(true)
        );
    }
}
