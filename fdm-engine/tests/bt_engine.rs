use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use fdm_core::SettingsStore;
use fdm_engine::{BtEngine, BtSource};
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session,
    SessionOptions,
};

#[tokio::test]
async fn bt_engine_inspects_and_adds_torrent() {
    let td = tempfile::tempdir().unwrap();
    let source_file = td.path().join("sample.bin");
    std::fs::write(&source_file, vec![7u8; 256 * 1024]).unwrap();

    let created = create_torrent(
        &source_file,
        CreateTorrentOptions {
            piece_length: Some(64 * 1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let bytes = created.as_bytes().unwrap().to_vec();
    let info_hash = created.info_hash().as_string();

    let settings_path = td.path().join("settings.json");
    let download_dir = td.path().join("downloads");
    std::fs::write(
        &settings_path,
        format!(
            r#"{{"defaultDir":{},"bt":{{"enabled":true,"listenPort":0,"dhtEnabled":false,"trackerEnabled":false,"maxPeers":20,"seedAfterComplete":false,"downloadDir":{}}}}}"#,
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
        ),
    )
    .unwrap();
    let settings = Arc::new(SettingsStore::load(td.path()).unwrap());
    let engine = BtEngine::new(td.path(), settings).await.unwrap();

    let info = engine.inspect_torrent(bytes.clone()).await.unwrap();
    assert_eq!(info.name, "sample.bin");
    assert_eq!(info.total, 256 * 1024);
    assert_eq!(info.files.len(), 1);
    assert_eq!(info.info_hash, info_hash);

    let added = engine
        .add(1, BtSource::TorrentBytes(bytes), &download_dir, true, None, None)
        .await
        .unwrap();
    assert_eq!(added.info_hash, info_hash);
    assert!(engine.stats(1).await.is_some());
    let _ = engine.pause(1).await;
    let _ = engine.resume(1).await;
    assert!(engine.remove(1, false).await);

    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bt_engine_downloads_from_local_peer() {
    let td = tempfile::tempdir().unwrap();
    let seed_root = td.path().join("seed");
    std::fs::create_dir_all(&seed_root).unwrap();
    let source_file = seed_root.join("payload.bin");
    std::fs::write(&source_file, vec![9u8; 128 * 1024]).unwrap();

    let created = create_torrent(
        &source_file,
        CreateTorrentOptions {
            piece_length: Some(32 * 1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let bytes = created.as_bytes().unwrap().to_vec();

    let seeder = Session::new_with_opts(
        seed_root.clone(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            listen: Some(ListenerOptions {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let seeder_handle = seeder
        .add_torrent(
            AddTorrent::from_bytes(bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(seed_root.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    seeder_handle.wait_until_initialized().await.unwrap();
    let seeder_addr = seeder.listen_addr().expect("seeder listen addr");

    let settings_path = td.path().join("settings.json");
    let download_dir = td.path().join("downloads");
    std::fs::write(
        &settings_path,
        format!(
            r#"{{"defaultDir":{},"bt":{{"enabled":true,"listenPort":0,"dhtEnabled":false,"trackerEnabled":false,"maxPeers":20,"seedAfterComplete":false,"downloadDir":{}}}}}"#,
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
        ),
    )
    .unwrap();
    let settings = Arc::new(SettingsStore::load(td.path()).unwrap());
    let engine = BtEngine::new(td.path(), settings).await.unwrap();
    engine
        .add(
            1,
            BtSource::TorrentBytes(bytes),
            &download_dir,
            false,
            Some(vec![seeder_addr]),
            None,
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(stats) = engine.stats(1).await {
            if stats.progress_bytes >= 128 * 1024 {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "local BT transfer did not complete in 30s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        std::fs::metadata(download_dir.join("payload.bin"))
            .unwrap()
            .len()
            >= 128 * 1024
    );
    engine.shutdown().await;
    seeder.stop().await;
}


#[tokio::test]
async fn bt_engine_reattaches_persisted_torrent_after_restart() {
    let td = tempfile::tempdir().unwrap();
    let source_file = td.path().join("restart.bin");
    std::fs::write(&source_file, vec![3u8; 128 * 1024]).unwrap();

    let created = create_torrent(
        &source_file,
        CreateTorrentOptions {
            piece_length: Some(32 * 1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let bytes = created.as_bytes().unwrap().to_vec();
    let info_hash = created.info_hash().as_string();

    let settings_path = td.path().join("settings.json");
    let download_dir = td.path().join("downloads");
    std::fs::write(
        &settings_path,
        format!(
            r#"{{"defaultDir":{},"bt":{{"enabled":true,"listenPort":0,"dhtEnabled":false,"trackerEnabled":false,"maxPeers":20,"seedAfterComplete":false,"downloadDir":{}}}}}"#,
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
            serde_json::to_string(&download_dir.to_string_lossy()).unwrap(),
        ),
    )
    .unwrap();

    // 第一次启动并添加任务
    {
        let settings = Arc::new(SettingsStore::load(td.path()).unwrap());
        let engine = BtEngine::new(td.path(), settings).await.unwrap();
        engine
            .add(
                42,
                BtSource::TorrentBytes(bytes.clone()),
                &download_dir,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(engine.stats(42).await.is_some());
        engine.shutdown().await;
    }

    // 模拟应用重启：重建 BtEngine 并使用 reattach 恢复任务关联
    {
        let settings = Arc::new(SettingsStore::load(td.path()).unwrap());
        let engine = BtEngine::new(td.path(), settings).await.unwrap();
        assert!(
            engine.reattach(42, &info_hash).await,
            "重启后应能通过 info_hash 重新挂载任务句柄"
        );
        let stats = engine.stats(42).await.expect("重新挂载后必须能获取状态");
        assert_eq!(stats.total_bytes, 128 * 1024);
        engine.shutdown().await;
    }
}
