use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;

pub const APP_DIR_NAME: &str = "FDM";
pub const DATA_DIR_NAME: &str = "data";
const SKIP_NAMES: &[&str] = &[".gitkeep", "_chrome_profile"];

pub fn home_dir() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn app_dir() -> PathBuf {
    home_dir().join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Some(custom) = env::var_os("FDM_DATA_DIR") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    app_dir().join(DATA_DIR_NAME)
}

pub fn resolve_data_dir(data_dir: Option<PathBuf>) -> PathBuf {
    match data_dir {
        None => self::data_dir(),
        Some(p) if p.is_absolute() => p,
        Some(p) => app_dir().join(p),
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

/// 将数据写入同目录临时文件，再原子替换目标文件。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

pub fn write_json_pretty<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    atomic_write(path, text.as_bytes())
}

pub fn migrate_legacy_data(target_data_dir: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = env::var("FDM_LEGACY_DATA") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join(DATA_DIR_NAME));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join(DATA_DIR_NAME));
    }

    let target = target_data_dir
        .canonicalize()
        .unwrap_or_else(|_| target_data_dir.to_path_buf());
    let mut migrated = Vec::new();

    for legacy in candidates {
        if !legacy.is_dir() {
            continue;
        }
        let legacy_abs = legacy.canonicalize().unwrap_or_else(|_| legacy.clone());
        if legacy_abs == target {
            continue;
        }
        let Ok(entries) = fs::read_dir(&legacy) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_NAMES.iter().any(|s| *s == name) {
                continue;
            }
            let dest = target_data_dir.join(&name);
            if dest.exists() {
                continue;
            }
            let src = entry.path();
            let copied = if src.is_dir() {
                copy_dir_all(&src, &dest).is_ok()
            } else {
                fs::copy(&src, &dest).is_ok()
            };
            if copied {
                migrated.push(name);
            }
        }
    }

    if !migrated.is_empty() {
        tracing::info!(?migrated, target = %target_data_dir.display(), "已迁移旧 FDM 数据");
    }
    migrated
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}
