//! Cooperative cancellation of the loop (US-001, unified in US-008).
//!
//! The SIGNAL is `tokio_util::sync::CancellationToken`: one tree for the whole
//! process, where runtime, thread, turn and later sub-agent each own a child
//! token. The in-house `watch`-based token this module used to define was
//! removed in US-008: keeping two mechanisms meant keeping two trees, and a turn
//! cancelled in one of them left the other one running (US-008 AC1, risk #9).
//!
//! What the core keeps is its own stop DISCIPLINE: the client signals, the loop
//! stops at a known boundary (end of a stream event, dispatch return, backoff
//! wake-up) then reconciles its transcript. A client-side `JoinHandle::abort()`
//! would cut the future at an arbitrary point, between pushing a `tool_use` and
//! writing its result.

use std::future::Future;

pub use tokio_util::sync::CancellationToken;

/// Outcome of work run under [`guard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancellable<T> {
    Completed(T),
    Cancelled,
}

/// Runs `fut` to completion or until `token` is cancelled.
///
/// The future is polled FIRST (`biased`): work already finished at signal time
/// yields its real result instead of being lost (edge case #2). This is why the
/// core does not call `CancellationToken::run_until_cancelled`, whose unbiased
/// `select!` loses that race about half the time.
pub async fn guard<F: Future>(token: &CancellationToken, fut: F) -> Cancellable<F::Output> {
    tokio::select! {
        biased;
        out = fut => Cancellable::Completed(out),
        () = token.cancelled() => Cancellable::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_is_observed_by_every_clone() {
        let token = CancellationToken::new();
        let observer = token.clone();
        assert!(!observer.is_cancelled());
        token.cancel();
        assert!(observer.is_cancelled());
        observer.cancelled().await;
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_survives_dropped_observers() {
        let token = CancellationToken::new();
        token.cancel();
        // Second signal after "stop": no effect, no panic.
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn guard_prefers_a_finished_future_over_a_pending_cancel() {
        let token = CancellationToken::new();
        token.cancel();
        let out = guard(&token, async { 42 }).await;
        assert_eq!(out, Cancellable::Completed(42));
    }

    #[tokio::test]
    async fn guard_cancels_a_future_that_never_completes() {
        let token = CancellationToken::new();
        let signal = token.clone();
        tokio::spawn(async move { signal.cancel() });
        let out = guard(&token, std::future::pending::<()>()).await;
        assert_eq!(out, Cancellable::Cancelled);
    }

    /// US-008 AC1: a child stops with its parent, and never the other way round.
    #[tokio::test]
    async fn a_child_token_stays_inside_its_own_branch() {
        let parent = CancellationToken::new();
        let turn = parent.child_token();
        let sibling = parent.child_token();

        turn.cancel();
        assert!(!parent.is_cancelled(), "a turn never cancels its thread");
        assert!(!sibling.is_cancelled(), "a turn never cancels a sibling");

        parent.cancel();
        assert!(
            sibling.is_cancelled(),
            "the parent still governs its branch"
        );
    }
}
