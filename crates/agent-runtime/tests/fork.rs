//! US-010: forking at a materialized turn boundary.
//!
//! The invariant under test is not "a copy exists" but "the copy is
//! independent": the parent keeps its bytes, the child keeps its provenance,
//! and neither writes into the other afterwards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use agent_runtime::event::{ForkOrigin, ThreadEventPayload};
use agent_runtime::id::{RandomIds, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::ThreadStore;
use agent_runtime::thread::{ForkError, Submission};
use common::{FakeProvider, FakeSession, Scripted, done_end_turn, text, wait_for_terminal};

fn one_turn() -> Vec<Scripted> {
    vec![
        Scripted::Stream(vec![text("premier"), done_end_turn()]),
        Scripted::Stream(vec![text("second"), done_end_turn()]),
    ]
}

async fn thread_with(turns: Vec<Scripted>) -> common::Harness {
    let provider = FakeProvider::new(turns);
    let deps = common::deps(provider, FakeSession::new(), Arc::new(common::EchoTools));
    common::start(Arc::new(RunAgentRunner::new(deps, common::agent_context))).await
}

/// AC1 + AC2: the cut is the terminal transition of a turn, and the child
/// carries the identity of its parent, of the turn and of the exact event.
#[tokio::test]
async fn a_fork_cuts_at_the_last_terminal_turn_and_carries_its_provenance() {
    let h = thread_with(one_turn()).await;
    let accepted = h.handle.submit(Submission::new("un")).await.unwrap();
    let terminal = wait_for_terminal(&h.handle).await;
    assert_eq!(terminal.state, TurnState::Completed);

    let before = h.store.read().await.unwrap().events;
    let fork = h
        .handle
        .fork(None)
        .await
        .expect("fork at the last terminal");

    assert_eq!(fork.fork_turn_id, accepted.turn_id);
    let cut = before
        .iter()
        .rev()
        .find(|e| {
            matches!(&e.payload, ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal())
        })
        .expect("the completed turn is durable");
    assert_eq!(fork.fork_event_id, cut.event_id);

    let child = fork.store.read().await.unwrap();
    assert_eq!(child.thread_id, Some(fork.child_thread_id));
    assert_eq!(
        child.origin,
        Some(ForkOrigin {
            parent_thread_id: h.thread_id,
            fork_turn_id: fork.fork_turn_id,
            fork_event_id: fork.fork_event_id,
        })
    );
    assert_eq!(
        child.events, before,
        "the branch holds the whole prefix through the cut"
    );

    // The parent records the branch, and only the parent does.
    let after = h.store.read().await.unwrap().events;
    assert_eq!(after.len(), before.len() + 1);
    assert!(matches!(
        after.last().map(|e| &e.payload),
        Some(ThreadEventPayload::Forked { child_thread_id, .. }) if *child_thread_id == fork.child_thread_id
    ));
    assert_eq!(
        fork.store.read().await.unwrap().events.len(),
        before.len(),
        "the `Forked` event belongs to the parent alone"
    );
}

/// AC3: after the cut, the two logs live their own life.
#[tokio::test]
async fn parent_and_child_diverge_without_cross_writes() {
    let h = thread_with(one_turn()).await;
    h.handle.submit(Submission::new("un")).await.unwrap();
    wait_for_terminal(&h.handle).await;
    let fork = h.handle.fork(None).await.expect("fork");
    let child_events = fork.store.read().await.unwrap().events.len();

    h.handle.submit(Submission::new("deux")).await.unwrap();
    wait_for_terminal(&h.handle).await;

    assert_eq!(
        fork.store.read().await.unwrap().events.len(),
        child_events,
        "a turn played on the parent writes nothing into the branch"
    );
    let parent = h.store.read().await.unwrap().events;
    assert!(
        parent.len() > child_events + 1,
        "the parent kept growing on its own"
    );
}

/// AC5, first branch: a moving transcript has no boundary to copy.
#[tokio::test]
async fn a_fork_is_refused_while_a_turn_is_running_and_writes_nothing() {
    let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("en cours")])]);
    let deps = common::deps(provider, FakeSession::new(), Arc::new(common::EchoTools));
    let h = common::start(Arc::new(RunAgentRunner::new(deps, common::agent_context))).await;

    let accepted = h.handle.submit(Submission::new("un")).await.unwrap();
    common::wait_for(
        || {
            h.handle
                .status()
                .turn
                .is_some_and(|t| t.state == TurnState::Running)
        },
        "the turn to be running",
    )
    .await;

    let before = h.store.read().await.unwrap().events;
    let err = h
        .handle
        .fork(None)
        .await
        .expect_err("a running turn refuses");
    assert!(
        matches!(&err, ForkError::TurnActive { current } if current.turn_id == accepted.turn_id),
        "expected TurnActive, got {err:?}"
    );
    assert_eq!(
        h.store.read().await.unwrap().events,
        before,
        "a refused fork appends nothing to the source"
    );

    h.handle.interrupt(None).await.unwrap();
    wait_for_terminal(&h.handle).await;
}

/// AC5, second branch: an identifier that belongs to no turn of this thread.
#[tokio::test]
async fn a_fork_at_an_unknown_turn_is_refused() {
    let h = thread_with(one_turn()).await;
    h.handle.submit(Submission::new("un")).await.unwrap();
    wait_for_terminal(&h.handle).await;

    let stranger = TurnId::generate(&RandomIds);
    let before = h.store.read().await.unwrap().events;
    let err = h
        .handle
        .fork(Some(stranger))
        .await
        .expect_err("unknown turn");
    assert!(
        matches!(err, ForkError::UnknownTurn { turn_id } if turn_id == stranger),
        "expected UnknownTurn, got {err:?}"
    );
    assert_eq!(h.store.read().await.unwrap().events, before);
}

/// A thread that never completed a turn has no boundary at all.
#[tokio::test]
async fn a_thread_without_a_terminal_turn_cannot_be_forked() {
    let h = thread_with(one_turn()).await;
    assert!(matches!(
        h.handle.fork(None).await,
        Err(ForkError::NoTerminalTurn)
    ));
    assert_eq!(
        h.store.read().await.unwrap().events.len(),
        1,
        "only the creation event is durable"
    );
}

/// Forking at a NAMED boundary cuts there, not at the latest one.
///
/// This is the rewind primitive of US-011: going back to an earlier turn is a
/// branch cut at that turn, and the source keeps everything that came after.
#[tokio::test]
async fn a_fork_at_a_named_turn_cuts_at_that_boundary() {
    let h = thread_with(one_turn()).await;
    let first = h.handle.submit(Submission::new("un")).await.unwrap();
    wait_for_terminal(&h.handle).await;
    let after_first = h.store.read().await.unwrap().events.len();
    let second = h.handle.submit(Submission::new("deux")).await.unwrap();
    wait_for_terminal(&h.handle).await;
    assert_ne!(first.turn_id, second.turn_id);

    let fork = h.handle.fork(Some(first.turn_id)).await.expect("fork");
    assert_eq!(fork.fork_turn_id, first.turn_id);
    let child = fork.store.read().await.unwrap();
    assert_eq!(
        child.events.len(),
        after_first,
        "the branch stops at the first turn, the second one stays in the parent"
    );
    assert!(
        !child
            .events
            .iter()
            .any(|e| e.turn_id() == Some(second.turn_id)),
        "no event of the later turn leaked into the branch"
    );
}
