//! `CancelToken`: COOPERATIVE cancellation signal of the loop (US-001).
//!
//! The core is responsible for its own shutdown: the client signals, the loop
//! stops at a known boundary (end of a stream event, dispatch return,
//! backoff wake-up) then reconciles its transcript. A client-side
//! `JoinHandle::abort()` would cut the future at an arbitrary point,
//! between pushing a `tool_use` and writing its result.
//!
//! Backing: `tokio::sync::watch`, already available through the workspace
//! `sync` feature: no new dependency (ADR-3: `Deps` stays a bag of injected
//! primitives).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

/// Clonable cancellation signal. Emitting and observing use the same type:
/// the core observes, the client emits, and the `Arc<Sender>` keeps the channel
/// open as long as a holder exists (no `changed()` returning an error).
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl CancelToken {
    /// New signal, not cancelled.
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::Sender::new(false)),
        }
    }

    /// Signals cancellation. Idempotent: a signal emitted after the loop has
    /// stopped produces neither a panic nor an observable effect.
    pub fn cancel(&self) {
        // `send_replace` (and not `send`): a channel without a live receiver (loop
        // already finished) must not surface an error.
        self.tx.send_replace(true);
    }

    /// Current state, without waiting. Used at loop boundaries.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves when cancellation is signalled. Stays pending otherwise.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow_and_update() {
                return;
            }
        }
        // Sender gone without cancellation: nothing can happen anymore.
        std::future::pending::<()>().await
    }

    /// Runs `fut` to completion or until cancellation. The future is polled
    /// FIRST (`biased`): work already finished at signal time yields its real
    /// result instead of being lost (edge case #2).
    pub async fn guard<F: Future>(&self, fut: F) -> Cancellable<F::Output> {
        tokio::select! {
            biased;
            out = fut => Cancellable::Completed(out),
            () = self.cancelled() => Cancellable::Cancelled,
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of work run under `CancelToken::guard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancellable<T> {
    Completed(T),
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_is_observed_by_every_clone() {
        let token = CancelToken::new();
        let observer = token.clone();
        assert!(!observer.is_cancelled());
        token.cancel();
        assert!(observer.is_cancelled());
        observer.cancelled().await;
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_survives_dropped_observers() {
        let token = CancelToken::new();
        token.cancel();
        // Second signal after "stop": no effect, no panic.
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn guard_prefers_a_finished_future_over_a_pending_cancel() {
        let token = CancelToken::new();
        token.cancel();
        let out = token.guard(async { 42 }).await;
        assert_eq!(out, Cancellable::Completed(42));
    }

    #[tokio::test]
    async fn guard_cancels_a_future_that_never_completes() {
        let token = CancelToken::new();
        let signal = token.clone();
        tokio::spawn(async move { signal.cancel() });
        let out = token.guard(std::future::pending::<()>()).await;
        assert_eq!(out, Cancellable::Cancelled);
    }
}
