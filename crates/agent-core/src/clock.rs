//! Horloge injectable (deps injectables, ARCHITECTURE §3.2) : la boucle ne lit
//! jamais l'heure système directement → tests déterministes, pas de `sleep` réel.

use std::time::Duration;

#[async_trait::async_trait]
pub trait Clock: Send + Sync {
    /// Maintenant en ms epoch.
    fn now_ms(&self) -> u64;
    /// Attend `dur` (backoff). En test, une implémentation no-op rend les tests
    /// instantanés.
    async fn sleep(&self, dur: Duration);
}

/// Horloge réelle (production).
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
