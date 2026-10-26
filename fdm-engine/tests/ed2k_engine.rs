#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;

use fdm_core::SettingsStore;
use fdm_engine::Ed2kEngine;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ed2k_engine_starts_and_manages_downloads() {
    if other_amule_running() {
        eprintln!("skip: another amuled.exe instance is already running");
        return;
    }
    let amule_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src-tauri/resources/amule/win-x64");
    if !amule_dir.join("amuled.exe").is_file() {
        eprintln!("skip: packaged aMule not found at {}", amule_dir.display());
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let downloads = td.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(
        td.path().join("settings.json"),
        format!(
            r#"{{"defaultDir":{},"ed2k":{{"enabled":true,"downloadDir":{},"tcpPort":15662,"udpPort":15672,"ecPort":15712,"apiPort":15713,"serverMetUrl":"","nodesDatUrl":"","shareCompleted":false,"downloadLimitKB":0}}}}"#,
            serde_json::to_string(&downloads.to_string_lossy()).unwrap(),
            serde_json::to_string(&downloads.to_string_lossy()).unwrap(),
        ),
    )
    .unwrap();
    let settings = Arc::new(SettingsStore::load(td.path()).unwrap());
    let engine = Ed2kEngine::new(td.path(), settings, amule_dir)
        .await
        .unwrap();

    let status = engine.status().await;
    assert!(status.ready);

    let link = "ed2k://|file|fdm-engine-test.bin|1024|0123456789abcdef0123456789abcdef|/";
    let hash = engine.add(1, link).await.unwrap();
    assert_eq!(hash, "0123456789abcdef0123456789abcdef");

    // Give aMule one refresher tick to surface the new partfile.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let stats = engine.stats(1).await;
    assert!(stats.is_some());

    let _ = engine.pause(1).await;
    let _ = engine.resume(1).await;
    assert!(engine.remove(1, false, "fdm-engine-test.bin").await);
    engine.shutdown().await;
}

#[cfg(windows)]
fn other_amule_running() -> bool {
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq amuled.exe"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .to_ascii_lowercase()
        .matches("amuled.exe")
        .count()
        > 0
}
