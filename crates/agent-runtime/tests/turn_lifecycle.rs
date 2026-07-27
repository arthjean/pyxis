//! US-005: `run_agent` runs inside an explicit turn lifecycle.
//!
//! Everything here is observed on the production seam: a real `ThreadHandle`
//! driving a real `RunAgentRunner` over fake I/O. What is proven is ORDER, which
//! is the only thing a durable lifecycle really buys: the running state is on
//! disk before the first provider call, the engine's own events cross untouched,
//! reconciliation lands before the terminal, and a failure leaves the thread
//! commandable with nothing running behind it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use agent_core::message::{ContentBlock, INTERRUPTED_TOOL_RESULT};
use agent_core::provider::ProviderError;
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::ThreadStore;
use agent_runtime::thread::{RuntimeEventPayload, Submission};

use common::{
    EchoTools, FakeProvider, FakeSession, HangingTools, Scripted, agent_context, collect_events,
    deps, done_end_turn, engine_labels, start, text, tool_call, wait_for, wait_for_terminal,
};

/// Ordered list of the durable orchestration events of a thread.
async fn durable(store: &dyn ThreadStore) -> Vec<ThreadEventPayload> {
    store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.payload)
        .collect()
}

/// AC1: `running` is durable BEFORE the first provider call, and AC2: every
/// engine event carries the thread, the turn and an event id without its content
/// being touched.
#[tokio::test]
async fn a_turn_is_durable_before_the_first_provider_call_and_events_stay_correlated() {
    let provider = FakeProvider::new(vec![Scripted::Stream(vec![
        text("bonjour"),
        done_end_turn(),
    ])]);
    let session = FakeSession::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            Arc::clone(&session),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());

    // The provider has not been called yet, and cannot be: the actor persists
    // `running` before it spawns the engine.
    assert_eq!(provider.calls(), 0);
    let accepted = harness
        .handle
        .submit(Submission::new("salut"))
        .await
        .expect("the submission is accepted");

    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.turn_id, accepted.turn_id);
    assert_eq!(turn.state, TurnState::Completed);

    let payloads = durable(harness.store.as_ref()).await;
    let running_at = payloads
        .iter()
        .position(|p| {
            matches!(
                p,
                ThreadEventPayload::TurnStateChanged {
                    to: TurnState::Running,
                    ..
                }
            )
        })
        .expect("the running transition is durable");
    let input_at = payloads
        .iter()
        .position(|p| matches!(p, ThreadEventPayload::InputSubmitted { .. }))
        .expect("the input is durable");
    assert!(input_at < running_at, "input then start: {payloads:?}");

    let seen = events.lock().unwrap().clone();
    let engine: Vec<_> = seen
        .iter()
        .filter(|e| matches!(e.payload, RuntimeEventPayload::Engine(_)))
        .collect();
    assert!(!engine.is_empty(), "the engine events crossed the seam");
    for event in engine {
        assert_eq!(event.thread_id, harness.thread_id);
        assert_eq!(
            event.turn_id,
            Some(accepted.turn_id),
            "every engine event names its turn"
        );
    }
    assert!(
        engine_labels(&seen).contains(&"text".to_string()),
        "the text of the model crossed unchanged: {:?}",
        engine_labels(&seen)
    );

    harness.handle.shutdown().await;
}

/// AC3: each engine outcome maps to exactly ONE terminal state, and it is the
/// last durable state of the turn.
#[tokio::test]
async fn every_engine_outcome_produces_exactly_one_terminal_state() {
    for (scripted, expected) in [
        (
            Scripted::Stream(vec![text("fini"), done_end_turn()]),
            TurnState::Completed,
        ),
        (
            Scripted::OpenErr(ProviderError::Http {
                status: 400,
                message: "mauvaise requête".into(),
                retry_after_ms: None,
            }),
            TurnState::Failed,
        ),
    ] {
        let provider = FakeProvider::failing(vec![scripted]);
        let session = FakeSession::new();
        let runner = Arc::new(RunAgentRunner::new(
            deps(provider, Arc::clone(&session), Arc::new(EchoTools)),
            agent_context,
        ));
        let harness = start(runner).await;

        harness
            .handle
            .submit(Submission::new("vas-y"))
            .await
            .unwrap();
        let turn = wait_for_terminal(&harness.handle).await;
        assert_eq!(turn.state, expected);

        harness.handle.shutdown().await;
        let terminals: Vec<TurnState> = durable(harness.store.as_ref())
            .await
            .into_iter()
            .filter_map(|p| match p {
                ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal() => Some(to),
                _ => None,
            })
            .collect();
        assert_eq!(terminals, vec![expected], "exactly one terminal");
    }
}

/// AC4: a tool call left without a result at interruption time already has its
/// synthetic result when the terminal is persisted. The reconciliation belongs
/// to the engine; what the runtime must not do is write the terminal first.
#[tokio::test]
async fn unanswered_tool_calls_are_reconciled_before_the_terminal_is_written() {
    let provider = FakeProvider::new(vec![Scripted::Stream(tool_call("call-1", "lent"))]);
    let session = FakeSession::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            Arc::clone(&session),
            Arc::new(HangingTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    harness
        .handle
        .submit(Submission::new("lance l'outil"))
        .await
        .unwrap();

    // Wait until the tool is actually in flight, then interrupt it.
    let in_flight = Arc::clone(&events);
    wait_for(
        move || engine_labels(&in_flight.lock().unwrap()).contains(&"tool_call".to_string()),
        "the tool call to reach the client",
    )
    .await;
    harness.handle.interrupt(None).await.unwrap();

    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.state, TurnState::Interrupted);

    let labels = engine_labels(&events.lock().unwrap());
    let synthetic = labels
        .iter()
        .position(|l| l == &format!("tool_result:{INTERRUPTED_TOOL_RESULT}"))
        .expect("the abandoned call got its synthetic result");
    let terminal = labels
        .iter()
        .position(|l| l == "interrupted")
        .expect("the engine emitted its terminal");
    assert!(
        synthetic < terminal,
        "reconciliation comes first: {labels:?}"
    );

    // And the transcript the engine persisted carries it, so the next turn is
    // not rejected for an orphan `tool_use`.
    let persisted = session.last();
    assert!(
        persisted.iter().flat_map(|m| &m.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { content, .. } if content == INTERRUPTED_TOOL_RESULT)
        }),
        "the synthetic result is in the transcript: {persisted:?}"
    );

    harness.handle.shutdown().await;
}

/// AC5: a provider failure makes the turn `failed`, leaves the thread
/// commandable and leaves NO engine running in the background: the next turn
/// starts, and it starts alone.
#[tokio::test]
async fn a_failed_turn_leaves_the_thread_commandable_and_nothing_running() {
    let provider = FakeProvider::failing(vec![
        Scripted::OpenErr(ProviderError::Http {
            status: 400,
            message: "refusé".into(),
            retry_after_ms: None,
        }),
        Scripted::Stream(vec![text("ça repart"), done_end_turn()]),
    ]);
    let session = FakeSession::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            Arc::clone(&session),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    harness
        .handle
        .submit(Submission::new("premier"))
        .await
        .unwrap();
    let failed = wait_for_terminal(&harness.handle).await;
    assert_eq!(failed.state, TurnState::Failed);

    // The thread still accepts work, and the second turn reaches its own
    // terminal: no zombie engine kept the slot.
    let second = harness
        .handle
        .submit(Submission::new("second"))
        .await
        .expect("the thread is still commandable after a failure");
    assert_ne!(second.turn_id, failed.turn_id);

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
    let terminals: Vec<TurnState> = durable(harness.store.as_ref())
        .await
        .into_iter()
        .filter_map(|p| match p {
            ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal() => Some(to),
            _ => None,
        })
        .collect();
    assert_eq!(
        terminals,
        vec![TurnState::Failed, TurnState::Completed],
        "one terminal per turn, in order"
    );
    assert_eq!(provider.calls(), 2, "one engine per turn, never two");
}
