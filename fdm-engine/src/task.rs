use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use fdm_core::{TaskData, TaskSnapshot};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::limiter::RateLimiter;

pub struct TaskEntry {
    pub id: i64,
    pub state: Mutex<TaskData>,
    pub limiter: RateLimiter,
    cancel: StdMutex<CancellationToken>,
    pub running: AtomicBool,
    /// 断点配置上次落盘时间，用于多 worker 共享节流。
    pub cfg_saved_at: StdMutex<Instant>,
}

impl TaskEntry {
    pub fn new(data: TaskData) -> Self {
        let id = data.id;
        let limiter = RateLimiter::new(data.max_rate);
        Self {
            id,
            state: Mutex::new(data),
            limiter,
            cancel: StdMutex::new(CancellationToken::new()),
            running: AtomicBool::new(false),
            cfg_saved_at: StdMutex::new(Instant::now()),
        }
    }

    pub fn reset_cancel_token(&self) -> CancellationToken {
        let token = CancellationToken::new();
        if let Ok(mut guard) = self.cancel.lock() {
            *guard = token.clone();
        }
        token
    }

    pub fn request_cancel(&self) {
        if let Ok(guard) = self.cancel.lock() {
            guard.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .lock()
            .map(|guard| guard.is_cancelled())
            .unwrap_or(false)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn mark_running(&self) -> bool {
        !self.running.swap(true, Ordering::SeqCst)
    }
    pub fn mark_stopped(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub async fn snapshot(&self) -> TaskSnapshot {
        let mut state = self.state.lock().await;
        state.refresh_progress();
        state.snapshot()
    }
}
