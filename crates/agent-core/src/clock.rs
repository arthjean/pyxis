//! Injectable clock (injectable deps, ARCHITECTURE 3.2): the loop never reads
//! the system time directly -> deterministic tests, no real `sleep`.

use std::time::Duration;

#[async_trait::async_trait]
pub trait Clock: Send + Sync {
    /// Now, in epoch ms.
    fn now_ms(&self) -> u64;
    /// Waits `dur` (backoff). In tests, a no-op implementation makes the tests
    /// instant.
    async fn sleep(&self, dur: Duration);
}

/// Real clock (production).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[async_trait::async_trait]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}
