use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub struct RateLimiter {
    max_rate: AtomicU64,
    next_time: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            max_rate: AtomicU64::new(bytes_per_sec),
            next_time: Mutex::new(Instant::now()),
        }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.max_rate.store(bytes_per_sec, Ordering::Relaxed);
        if let Ok(mut next) = self.next_time.try_lock() {
            *next = Instant::now();
        }
    }

    pub fn rate(&self) -> u64 {
        self.max_rate.load(Ordering::Relaxed)
    }

    /// 返回 false 表示等待期间收到暂停/取消。
    pub async fn acquire(&self, bytes: usize, token: &CancellationToken) -> bool {
        let rate = self.rate();
        if rate == 0 || bytes == 0 {
            return !token.is_cancelled();
        }
        let interval = Duration::from_secs_f64(bytes as f64 / rate as f64);
        let wait = {
            let mut next = self.next_time.lock().await;
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + interval;
            start.saturating_duration_since(now)
        };
        if wait.is_zero() {
            return !token.is_cancelled();
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => !token.is_cancelled(),
            _ = token.cancelled() => false,
        }
    }
}
