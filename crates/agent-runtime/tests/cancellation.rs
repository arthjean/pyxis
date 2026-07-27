//! US-001: the cancellation tree the in-house `CancelToken` cannot express.
//!
//! Two properties decide whether `tokio-util` replaces the in-house mechanism
//! instead of doubling it: a parent must stop its tracked descendants AND let
//! their destructors run before shutdown returns, and a child must never reach
//! its parent or its siblings.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Observes that the task's frame was really dropped, not merely that the task
/// stopped being polled.
struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancelling_a_parent_ends_a_tracked_child_and_runs_its_destructor() {
    let parent = CancellationToken::new();
    let child = parent.child_token();
    let tracker = TaskTracker::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let observed_cancel = Arc::new(AtomicBool::new(false));

    tracker.spawn({
        let guard = DropSignal(Arc::clone(&dropped));
        let child = child.clone();
        let observed_cancel = Arc::clone(&observed_cancel);
        async move {
            let _guard = guard;
            child.cancelled().await;
            observed_cancel.store(true, Ordering::SeqCst);
        }
    });

    // Give the task a chance to park on its token before cancelling.
    tokio::task::yield_now().await;
    assert!(
        !dropped.load(Ordering::SeqCst),
        "the child is still running"
    );

    parent.cancel();
    tracker.close();
    tokio::time::timeout(Duration::from_secs(2), tracker.wait())
        .await
        .expect("a cancelled tracked task must finish inside the shutdown budget");

    assert!(
        observed_cancel.load(Ordering::SeqCst),
        "the child observed the parent cancellation"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the child destructor ran before the end of the test"
    );
    assert!(tracker.is_empty(), "no task is left tracked");
}

#[tokio::test]
async fn cancelling_a_child_leaves_its_parent_and_its_sibling_running() {
    let parent = CancellationToken::new();
    let first = parent.child_token();
    let sibling = parent.child_token();

    first.cancel();

    assert!(first.is_cancelled());
    assert!(!sibling.is_cancelled(), "a sibling must stay active");
    assert!(
        !parent.is_cancelled(),
        "a child must never cancel its parent"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sibling.cancelled())
            .await
            .is_err(),
        "the sibling token is still pending"
    );

    // The parent still governs the branch that was not cancelled.
    parent.cancel();
    assert!(sibling.is_cancelled());
}

#[tokio::test]
async fn a_cloned_token_shares_its_cancellation_domain() {
    // A clone must NOT create an independent domain: US-008 depends on it (no
    // accidental orphan branch when a token is passed around by value).
    let root = CancellationToken::new();
    let clone = root.clone();
    let child_of_clone = clone.child_token();

    root.cancel();

    assert!(clone.is_cancelled());
    assert!(child_of_clone.is_cancelled());
}
