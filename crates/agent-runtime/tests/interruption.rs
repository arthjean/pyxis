//! US-008: one cancellation tree, one terminal.
//!
//! An interruption is acknowledged in the time it takes to flip a token, the
//! branch it cancels is its own, every cooperative point of the engine exits
//! through it, a task that ignores it is force-stopped inside the budget, and
//! whatever the path there is exactly one terminal.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::provider::ProviderError;
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::ThreadStore;
use agent_runtime::thread::{SHUTDOWN_DEADLINE, Submission};

use common::{
    EchoTools, FakeProvider, FakeSession, HangingTools, Scripted, agent_context, collect_events,
    deps, done_end_turn, engine_labels, start, text, tool_call, wait_for, wait_for_terminal,
};

async fn terminals(store: &dyn ThreadStore) -> Vec<(TurnState, Option<String>)> {
    store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.payload {
            ThreadEventPayload::TurnStateChanged { to, cause, .. } if to.is_terminal() => {
                Some((to, cause))
            }
            _ => None,
        })
        .collect()
}

/// AC2: the acknowledgement does not wait for the model to stop, and the
/// cancellation never leaves its own branch.
#[tokio::test]
async fn an_interruption_is_acknowledged_without_waiting_and_stays_in_its_branch() {
    let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("je réfléchis")])]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    let accepted = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();

    let streaming = Arc::clone(&events);
    wait_for(
        move || engine_labels(&streaming.lock().unwrap()).contains(&"text".to_string()),
        "the model to start talking",
    )
    .await;

    let before = Instant::now();
    let signalled = harness
        .handle
        .interrupt(Some(accepted.turn_id))
        .await
        .unwrap();
    let admission = before.elapsed();
    assert!(
        admission < Duration::from_millis(100),
        "acknowledged in {admission:?}, budget 100 ms"
    );
    assert_eq!(signalled.map(|t| t.turn_id), Some(accepted.turn_id));
    assert!(
        !harness.root.is_cancelled(),
        "a turn interruption never reaches the domain the thread hangs from"
    );

    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.state, TurnState::Interrupted);

    harness.handle.shutdown().await;
}

/// AC3: every cooperative point of the engine exits through the same token. A
/// backoff is the least obvious one: it is a `sleep` the loop must not finish.
#[tokio::test]
async fn a_turn_cancelled_during_a_backoff_reaches_its_terminal() {
    // The stream fails mid-flight, so the engine enters its retry backoff.
    let provider = FakeProvider::new(vec![
        Scripted::StreamThenErr(
            vec![text("je commence")],
            ProviderError::Http {
                status: 429,
                message: "slow down".into(),
                retry_after_ms: Some(30_000),
            },
        ),
        Scripted::StreamThenHang(Vec::new()),
    ]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();

    let resetting = Arc::clone(&events);
    wait_for(
        move || engine_labels(&resetting.lock().unwrap()).contains(&"stream_reset".to_string()),
        "the engine to enter its retry",
    )
    .await;

    harness.handle.interrupt(None).await.unwrap();
    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.state, TurnState::Interrupted);

    harness.handle.shutdown().await;
    assert_eq!(
        terminals(harness.store.as_ref()).await.len(),
        1,
        "exactly one terminal"
    );
}

/// AC5 and AC6: a turn that ignores its token is aborted inside the grace
/// period, its terminal is written once, and interrupting again changes nothing.
#[tokio::test]
async fn a_turn_that_ignores_cancellation_is_force_stopped_once() {
    // `HangingTools` never returns and never watches a token: dropping its
    // future is the only way out, and the engine cannot drop it because the
    // dispatch is what it is awaiting.
    let provider = FakeProvider::new(vec![Scripted::Stream(tool_call("call-1", "bloquant"))]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(HangingTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();

    let running = Arc::clone(&events);
    wait_for(
        move || engine_labels(&running.lock().unwrap()).contains(&"tool_call".to_string()),
        "the blocking tool to start",
    )
    .await;

    let before = Instant::now();
    harness.handle.interrupt(None).await.unwrap();
    // A second interruption, and one more after the terminal: both are no-ops.
    harness.handle.interrupt(None).await.unwrap();

    let turn = wait_for_terminal(&harness.handle).await;
    let stop = before.elapsed();
    assert_eq!(turn.state, TurnState::Interrupted);
    assert!(
        stop < SHUTDOWN_DEADLINE,
        "force-stopped in {stop:?}, budget {SHUTDOWN_DEADLINE:?}"
    );

    assert_eq!(
        harness.handle.interrupt(None).await.unwrap(),
        None,
        "interrupting a finished turn is a no-op"
    );

    harness.handle.shutdown().await;
    let terminals = terminals(harness.store.as_ref()).await;
    assert_eq!(terminals.len(), 1, "one terminal, not two: {terminals:?}");
    assert_eq!(terminals[0].0, TurnState::Interrupted);
}

/// A turn interrupted before it ever produced a terminal frees the slot: the
/// thread stays commandable and the next turn runs alone.
#[tokio::test]
async fn the_thread_stays_commandable_after_an_interruption() {
    let provider = FakeProvider::new(vec![
        Scripted::StreamThenHang(vec![text("je réfléchis")]),
        Scripted::Stream(vec![text("deuxième"), done_end_turn()]),
    ]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    let first = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();
    let streaming = Arc::clone(&events);
    wait_for(
        move || engine_labels(&streaming.lock().unwrap()).contains(&"text".to_string()),
        "the model to start talking",
    )
    .await;

    harness.handle.interrupt(None).await.unwrap();
    wait_for_terminal(&harness.handle).await;

    let second = harness
        .handle
        .submit(Submission::new("réessaie"))
        .await
        .unwrap();
    assert_ne!(second.turn_id, first.turn_id);
    let handle = Arc::clone(&harness.handle);
    wait_for(
        move || {
            handle
                .status()
                .turn
                .is_some_and(|t| t.turn_id == second.turn_id && t.state == TurnState::Completed)
        },
        "the second turn to complete",
    )
    .await;

    harness.handle.shutdown().await;
    let terminals = terminals(harness.store.as_ref()).await;
    assert_eq!(
        terminals
            .iter()
            .map(|(state, _)| *state)
            .collect::<Vec<_>>(),
        vec![TurnState::Interrupted, TurnState::Completed]
    );
}

/// AC1: interrupting one thread leaves a sibling thread of the same runtime
/// untouched, and the runtime root still governs both.
#[tokio::test]
async fn interrupting_one_thread_leaves_its_sibling_running() {
    let make = || {
        let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("...")])]);
        Arc::new(RunAgentRunner::new(
            deps(provider, FakeSession::new(), Arc::new(EchoTools)),
            agent_context,
        ))
    };

    let first = start(make()).await;
    let second = start(make()).await;
    first.handle.submit(Submission::new("un")).await.unwrap();
    second.handle.submit(Submission::new("deux")).await.unwrap();

    first.handle.interrupt(None).await.unwrap();
    let interrupted = wait_for_terminal(&first.handle).await;
    assert_eq!(interrupted.state, TurnState::Interrupted);

    // The sibling never learned about it.
    assert!(
        second
            .handle
            .status()
            .turn
            .is_some_and(|t| t.state == TurnState::Running),
        "the sibling turn is still running"
    );
    assert!(!second.root.is_cancelled());

    first.handle.shutdown().await;
    second.handle.shutdown().await;
}
