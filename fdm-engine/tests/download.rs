use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fdm_core::SettingsStore;
use fdm_engine::TaskManager;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn body() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8).collect()
}

async fn spawn_range_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(body());
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = data.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let Ok(n) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..n]);
                let mut lines = request.lines();
                let first = lines.next().unwrap_or("");
                let is_head = first.starts_with("HEAD ");
                let mut range: Option<(usize, usize)> = None;
                for line in lines {
                    if line.to_ascii_lowercase().starts_with("range:") {
                        let raw = line.split(':').nth(1).unwrap_or("").trim();
                        if let Some(value) = raw.strip_prefix("bytes=") {
                            let parts: Vec<_> = value.split('-').collect();
                            if parts.len() == 2 {
                                let start = parts[0].parse::<usize>().unwrap_or(0);
                                let end = if parts[1].is_empty() {
                                    data.len() - 1
                                } else {
                                    parts[1].parse::<usize>().unwrap_or(data.len() - 1)
                                };
                                range = Some((start, end.min(data.len() - 1)));
                            }
                        }
                    }
                }
                let total = data.len();
                if is_head {
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Disposition: attachment; filename*=UTF-8''test-%E4%B8%AD%E6%96%87.bin\r\nConnection: close\r\n\r\n", total);
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                if let Some((start, end)) = range {
                    let slice = &data[start..=end];
                    let response = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len(), start, end, total);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(slice).await;
                } else {
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", total);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&data).await;
                }
            });
        }
    });
    (format!("http://{}/archive.bin", addr), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_segment_download_completes_and_merges() {
    let (url, server) = spawn_range_server().await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir.clone()), settings).unwrap();
    manager.start_scheduler();

    let snap = manager
        .add_task(&json!({
            "url": url,
            "dir": download_dir.to_string_lossy(),
            "connections": 4,
            "start": true
        }))
        .await
        .unwrap();

    let mut final_snap = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Some(snap) = manager.get_task(snap.id).await {
            if snap.status == "done" || snap.status == "error" {
                final_snap = Some(snap);
                break;
            }
        }
    }
    let snap = final_snap.expect("download did not finish");
    assert_eq!(snap.status, "done", "error: {:?}", snap.error);
    let path = PathBuf::from(&snap.filepath);
    assert_eq!(path.file_name().unwrap().to_string_lossy(), "test-中文.bin");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes, body());
    assert!(!PathBuf::from(format!("{}.fdmpart", path.to_string_lossy())).exists());
    assert!(!PathBuf::from(format!("{}.fdmcfg", path.to_string_lossy())).exists());
    server.abort();
}

async fn spawn_no_range_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(body());
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = data.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let Ok(n) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..n]);
                let is_head = request.starts_with("HEAD ");
                let total = data.len();
                if is_head {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        total
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        total
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&data).await;
                }
            });
        }
    });
    (format!("http://{}/single.bin", addr), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_stream_download_without_range_support() {
    let (url, server) = spawn_no_range_server().await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings).unwrap();
    manager.start_scheduler();
    let snap = manager
        .add_task(&json!({"url": url, "dir": download_dir.to_string_lossy(), "connections": 8}))
        .await
        .unwrap();
    let mut final_snap = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        if let Some(s) = manager.get_task(snap.id).await {
            if s.status == "done" || s.status == "error" {
                final_snap = Some(s);
                break;
            }
        }
    }
    let snap = final_snap.expect("no-range download did not finish");
    assert_eq!(snap.status, "done", "error: {:?}", snap.error);
    assert_eq!(std::fs::read(&snap.filepath).unwrap(), body());
    server.abort();
}

async fn spawn_chunked_probe_server(fail_first_download: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(body());
    let download_attempts = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = data.clone();
            let download_attempts = download_attempts.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let Ok(n) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..n]);
                let is_head = request.starts_with("HEAD ");
                let has_range = request
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("range:"));
                if is_head {
                    // 探测拿不到大小：既没有 Content-Length，也没有 Accept-Ranges。
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                if has_range {
                    // Range 探测同样拿不到大小。
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                let attempt = download_attempts.fetch_add(1, Ordering::SeqCst);
                if fail_first_download && attempt == 0 {
                    let error_body = b"service unavailable";
                    let response = format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        error_body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(error_body).await;
                    return;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                for chunk in data.chunks(32 * 1024) {
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
            });
        }
    });
    (format!("http://{}/unknown-size.bin", addr), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_stream_learns_size_from_download_response() {
    let (url, server) = spawn_chunked_probe_server(false).await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings).unwrap();
    manager.start_scheduler();
    let snap = manager
        .add_task(&json!({"url": url, "dir": download_dir.to_string_lossy(), "connections": 8}))
        .await
        .unwrap();
    let mut saw_size_while_downloading = false;
    let mut final_snap = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(s) = manager.get_task(snap.id).await {
            if s.status == "downloading" && s.total == Some(body().len() as u64) {
                saw_size_while_downloading = true;
            }
            if s.status == "done" || s.status == "error" {
                final_snap = Some(s);
                break;
            }
        }
    }
    assert!(
        saw_size_while_downloading,
        "下载中应从响应头补全文件大小"
    );
    let snap = final_snap.expect("unknown-size download did not finish");
    assert_eq!(snap.status, "done", "error: {:?}", snap.error);
    assert_eq!(snap.total, Some(body().len() as u64));
    assert_eq!(std::fs::read(&snap.filepath).unwrap(), body());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_stream_ignores_error_response_size() {
    let (url, server) = spawn_chunked_probe_server(true).await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings).unwrap();
    manager.start_scheduler();
    let snap = manager
        .add_task(&json!({"url": url, "dir": download_dir.to_string_lossy(), "connections": 8}))
        .await
        .unwrap();
    let mut saw_error_size = false;
    let mut final_snap = None;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(s) = manager.get_task(snap.id).await {
            if s.total == Some(b"service unavailable".len() as u64) {
                saw_error_size = true;
            }
            if s.status == "done" || s.status == "error" {
                final_snap = Some(s);
                break;
            }
        }
    }
    assert!(!saw_error_size, "错误响应的大小不应写入任务");
    let snap = final_snap.expect("download after error response did not finish");
    assert_eq!(snap.status, "done", "error: {:?}", snap.error);
    assert_eq!(snap.total, Some(body().len() as u64));
    assert_eq!(std::fs::read(&snap.filepath).unwrap(), body());
    server.abort();
}

fn large_body() -> Vec<u8> {
    (0..2_000_000u32).map(|i| (i % 253) as u8).collect()
}

async fn spawn_slow_range_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(large_body());
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = data.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let Ok(n) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..n]);
                if request.starts_with("HEAD ") {
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", data.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                let mut range = None;
                for line in request.lines() {
                    if line.to_ascii_lowercase().starts_with("range:") {
                        let value = line
                            .split(':')
                            .nth(1)
                            .unwrap_or("")
                            .trim()
                            .strip_prefix("bytes=")
                            .unwrap_or("");
                        let parts: Vec<_> = value.split('-').collect();
                        if parts.len() == 2 {
                            let start = parts[0].parse::<usize>().unwrap_or(0);
                            let end = if parts[1].is_empty() {
                                data.len() - 1
                            } else {
                                parts[1].parse::<usize>().unwrap_or(data.len() - 1)
                            };
                            range = Some((start, end.min(data.len() - 1)));
                        }
                    }
                }
                if let Some((start, end)) = range {
                    let slice = &data[start..=end];
                    let response = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len(), start, end, data.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                    for chunk in slice.chunks(16384) {
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            });
        }
    });
    (format!("http://{}/slow.bin", addr), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_keeps_partial_progress() {
    let (url, server) = spawn_slow_range_server().await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings).unwrap();
    manager.start_scheduler();
    let snap = manager.add_task(&json!({"url": url, "dir": download_dir.to_string_lossy(), "filename": "slow.bin", "connections": 4})).await.unwrap();

    let mut started = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(s) = manager.get_task(snap.id).await {
            if s.status == "downloading" && s.downloaded > 0 {
                started = true;
                break;
            }
        }
    }
    assert!(started, "download never started");
    assert!(manager.pause_task(snap.id).await);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let paused = manager.get_task(snap.id).await.unwrap();
    assert_eq!(paused.status, "paused");
    assert!(paused.downloaded > 0);
    assert!(paused.downloaded < paused.total.unwrap());
    // 暂停时必须强制 flush：每个分片已计入 downloaded 的字节都应已经写在 .fdmpart 中。
    let part_path = PathBuf::from(format!("{}.fdmpart", paused.filepath));
    let part = std::fs::read(&part_path).unwrap();
    let expected = large_body();
    for segment in &paused.segments {
        let end = segment.cur.min(segment.end.saturating_add(1));
        if end > segment.start {
            let start = segment.start as usize;
            let end = end as usize;
            assert_eq!(
                &part[start..end],
                &expected[start..end],
                "segment {} not flushed",
                segment.index
            );
        }
    }

    assert!(manager.resume_task(snap.id).await);
    let mut done = None;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(80)).await;
        if let Some(s) = manager.get_task(snap.id).await {
            if s.status == "done" || s.status == "error" {
                done = Some(s);
                break;
            }
        }
    }
    let done = done.expect("resumed download did not finish");
    assert_eq!(done.status, "done", "error: {:?}", done.error);
    assert_eq!(std::fs::read(&done.filepath).unwrap(), large_body());
    server.abort();
}

async fn spawn_keepalive_range_server(
    data: Vec<u8>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(data);
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_out = accepts.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            accepts.fetch_add(1, Ordering::SeqCst);
            let data = data.clone();
            tokio::spawn(async move {
                loop {
                    let mut buffer = vec![0u8; 16384];
                    let Ok(n) = socket.read(&mut buffer).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let request = String::from_utf8_lossy(&buffer[..n]);
                    let first = request.lines().next().unwrap_or("");
                    let is_head = first.starts_with("HEAD ");
                    let total = data.len();
                    if is_head {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                            total
                        );
                        if socket.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let mut range: Option<(usize, usize)> = None;
                    for line in request.lines() {
                        if line.to_ascii_lowercase().starts_with("range:") {
                            let value = line
                                .split(':')
                                .nth(1)
                                .unwrap_or("")
                                .trim()
                                .strip_prefix("bytes=")
                                .unwrap_or("");
                            let parts: Vec<_> = value.split('-').collect();
                            if parts.len() == 2 {
                                let start = parts[0].parse::<usize>().unwrap_or(0);
                                let end = if parts[1].is_empty() {
                                    total - 1
                                } else {
                                    parts[1].parse::<usize>().unwrap_or(total - 1)
                                };
                                range = Some((start, end.min(total - 1)));
                            }
                        }
                    }
                    let (status, start, end, slice): (&str, usize, usize, &[u8]) =
                        if let Some((start, end)) = range {
                            ("206 Partial Content", start, end, &data[start..=end])
                        } else {
                            ("200 OK", 0, total - 1, &data[..])
                        };
                    let content_range = if status.starts_with("206") {
                        format!("Content-Range: bytes {}-{}/{}\r\n", start, end, total)
                    } else {
                        String::new()
                    };
                    let response = format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\n{}Accept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                        status,
                        slice.len(),
                        content_range
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                    if socket.write_all(slice).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (
        format!("http://{}/keepalive.bin", addr),
        accepts_out,
        handle,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_client_reuses_connection_pool_across_tasks() {
    let (url, accepts, server) = spawn_keepalive_range_server(body()).await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings.clone()).unwrap();
    manager.start_scheduler();

    let first_client = manager.http_client().unwrap();
    let second_client = manager.http_client().unwrap();
    assert!(Arc::ptr_eq(&first_client, &second_client));

    for index in 0..3 {
        let snap = manager
            .add_task(&json!({
                "url": url,
                "dir": download_dir.to_string_lossy(),
                "filename": format!("reuse-{index}.bin"),
                "connections": 2,
                "start": true
            }))
            .await
            .unwrap();
        let mut finished = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(snapshot) = manager.get_task(snap.id).await {
                if snapshot.status == "done" || snapshot.status == "error" {
                    finished = Some(snapshot);
                    break;
                }
            }
        }
        let finished = finished.expect("reuse download did not finish");
        assert_eq!(finished.status, "done", "error: {:?}", finished.error);
        assert_eq!(std::fs::read(&finished.filepath).unwrap(), body());
    }
    // 旧实现每个任务、每个分片都会新建 Client；共享后 3 个顺序任务应只保留少量连接。
    assert!(
        accepts.load(Ordering::SeqCst) <= 6,
        "too many TCP accepts: {}",
        accepts.load(Ordering::SeqCst)
    );

    settings
        .update_patch(&json!({"proxyEnabled": true, "proxyHost": "127.0.0.1", "proxyPort": "9"}))
        .unwrap();
    let changed_client = manager.http_client().unwrap();
    assert!(!Arc::ptr_eq(&first_client, &changed_client));
    server.abort();
}

fn skewed_body() -> Vec<u8> {
    (0..16_000_000u32).map(|i| (i % 247) as u8).collect()
}

async fn spawn_skewed_range_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(skewed_body());
    let total = data.len();
    let slow_start = total / 2;
    let slow_end = total * 3 / 4;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let data = data.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let Ok(n) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..n]);
                if request.starts_with("HEAD ") {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                let mut range = None;
                for line in request.lines() {
                    if line.to_ascii_lowercase().starts_with("range:") {
                        let value = line
                            .split(':')
                            .nth(1)
                            .unwrap_or("")
                            .trim()
                            .strip_prefix("bytes=")
                            .unwrap_or("");
                        let parts: Vec<_> = value.split('-').collect();
                        if parts.len() == 2 {
                            let start = parts[0].parse::<usize>().unwrap_or(0);
                            let end = if parts[1].is_empty() {
                                data.len() - 1
                            } else {
                                parts[1].parse::<usize>().unwrap_or(data.len() - 1)
                            };
                            range = Some((start, end.min(data.len() - 1)));
                        }
                    }
                }
                let Some((start, end)) = range else {
                    return;
                };
                let slice = &data[start..=end];
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start,
                    end,
                    data.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let slow = start >= slow_start && start < slow_end;
                for chunk in slice.chunks(16384) {
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                    if slow {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            });
        }
    });
    (format!("http://{}/skewed.bin", addr), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_stealing_splits_slow_tail_and_keeps_connection_count() {
    let (url, server) = spawn_skewed_range_server().await;
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    let settings = Arc::new(SettingsStore::load(&data_dir).unwrap());
    settings
        .update_patch(&json!({"defaultDir": download_dir.to_string_lossy()}))
        .unwrap();
    let manager = TaskManager::new(Some(data_dir), settings).unwrap();
    manager.start_scheduler();

    let started = Instant::now();
    let snap = manager
        .add_task(&json!({
            "url": url,
            "dir": download_dir.to_string_lossy(),
            "filename": "skewed.bin",
            "connections": 4,
            "start": true
        }))
        .await
        .unwrap();
    let mut finished = None;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(snapshot) = manager.get_task(snap.id).await {
            if snapshot.status == "done" || snapshot.status == "error" {
                finished = Some(snapshot);
                break;
            }
        }
    }
    let finished = finished.expect("skewed download did not finish");
    assert_eq!(finished.status, "done", "error: {:?}", finished.error);
    assert_eq!(std::fs::read(&finished.filepath).unwrap(), skewed_body());
    assert!(
        finished.segments.len() > 4,
        "work stealing did not create extra segments"
    );
    assert_eq!(
        finished.connections, 4,
        "UI connection count must stay at configured value"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "tail download was too slow"
    );
    server.abort();
}
