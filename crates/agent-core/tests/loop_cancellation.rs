//! Cooperative cancellation and transcript reconciliation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::CancellationToken;
use agent_core::message::INTERRUPTED_TOOL_RESULT;
use agent_core::message::Message;
use agent_core::message::unanswered_tool_calls;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use std::sync::Arc;

mod common;

use common::{
    DelayedCancelTools, HangingTools, MockTurn, RacingTools, drive, harness,
    persisted_tool_results, text_turn, tool_results, tool_turn, tool_turn_n,
};

// US-001 AC4: signal already set on entry -> stop at the first boundary,
// `Interrupted` emitted by the CORE, no provider call.
#[tokio::test]
async fn cancel_before_start_stops_at_the_first_boundary() {
    let h = harness(vec![text_turn("jamais")], false, 100_000);
    let mut deps = h.deps.clone();
    let cancel = CancellationToken::new();
    cancel.cancel();
    deps.cancel = cancel;
    let events = drive(AgentContext::new("mock").push(Message::user("hello")), deps).await;
    assert!(
        matches!(events.as_slice(), [AgentEvent::Interrupted(..)]),
        "single Interrupted expected: {events:?}"
    );
    assert!(
        !h.log.lock().unwrap().contains(&"stream"),
        "provider should not be called: {:?}",
        h.log.lock().unwrap()
    );
}

// US-001 AC2: cancellation during streaming -> no `Text` after the
// boundary; whatever already scrolled by stays in the persisted transcript.
#[tokio::test]
async fn cancel_during_stream_stops_emitting_deltas() {
    let cancel = CancellationToken::new();
    let h = harness(
        vec![MockTurn::StreamCancelling(
            vec![
                StreamEvent::TextDelta {
                    text: "premier".into(),
                },
                StreamEvent::TextDelta {
                    text: "second".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                },
            ],
            cancel.clone(),
        )],
        false,
        100_000,
    );
    let mut deps = h.deps.clone();
    deps.cancel = cancel;
    let events = drive(AgentContext::new("mock").push(Message::user("hi")), deps).await;
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["premier"], "stream drained after cancel");
    assert!(
        matches!(events.last(), Some(AgentEvent::Interrupted(..))),
        "{events:?}"
    );
    let synced = h.boundaries.synced.lock().unwrap().clone();
    assert!(
        synced.iter().any(|m| m.text().contains("premier")),
        "partial answer kept: {synced:?}"
    );
}

// US-001 AC3 + US-002 AC1/AC3: interruption during a dispatch that never
// hands back control -> the loop takes control back, writes a synthetic result
// per in-flight call, THEN persists.
#[tokio::test]
async fn interrupted_dispatch_writes_synthetic_results_before_persisting() {
    let cancel = CancellationToken::new();
    let h = harness(vec![tool_turn("c1")], false, 100_000);
    let mut deps = h.deps.clone();
    deps.cancel = cancel.clone();
    deps.tools = Arc::new(HangingTools(cancel));
    let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

    assert!(
        matches!(events.last(), Some(AgentEvent::Interrupted(..))),
        "{events:?}"
    );
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "{events:?}");
    assert_eq!(results[0].id, "c1");
    assert!(results[0].is_error, "interrupted result is an error");
    assert_eq!(results[0].content, INTERRUPTED_TOOL_RESULT);

    let synced = h.boundaries.synced.lock().unwrap().clone();
    assert!(
        unanswered_tool_calls(&synced).is_empty(),
        "no orphan call in the persisted transcript: {synced:?}"
    );
    assert_eq!(
        persisted_tool_results(&synced),
        vec![INTERRUPTED_TOOL_RESULT.to_string()],
        "the synthetic result is persisted: {synced:?}"
    );
}

// US-002 AC5: several concurrent calls -> exactly one result per call,
// with no duplicate and none forgotten.
#[tokio::test]
async fn interrupted_concurrent_dispatch_answers_every_call_once() {
    let cancel = CancellationToken::new();
    let h = harness(vec![tool_turn_n(&["c1", "c2"])], false, 100_000);
    let mut deps = h.deps.clone();
    deps.cancel = cancel.clone();
    deps.tools = Arc::new(HangingTools(cancel));
    let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

    let ids: Vec<&str> = tool_results(&events)
        .iter()
        .map(|v| v.id.as_str())
        .collect();
    assert_eq!(ids, vec!["c1", "c2"], "{events:?}");
    let synced = h.boundaries.synced.lock().unwrap().clone();
    assert!(unanswered_tool_calls(&synced).is_empty(), "{synced:?}");
    assert_eq!(
        persisted_tool_results(&synced),
        vec![
            INTERRUPTED_TOOL_RESULT.to_string(),
            INTERRUPTED_TOOL_RESULT.to_string()
        ],
        "exactly one result per call: {synced:?}"
    );
}

// Edge case #2: the dispatch completes in the same window as the cancellation
// -> the REAL result is kept, no synthetic result overwrites it.
#[tokio::test]
async fn tool_finished_before_stop_keeps_its_real_result() {
    let cancel = CancellationToken::new();
    let h = harness(vec![tool_turn("c1")], false, 100_000);
    let mut deps = h.deps.clone();
    deps.cancel = cancel.clone();
    deps.tools = Arc::new(RacingTools(cancel));
    let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "{events:?}");
    assert_eq!(results[0].content, "real output");
    assert!(!results[0].is_error);
    let synced = h.boundaries.synced.lock().unwrap().clone();
    assert_eq!(
        persisted_tool_results(&synced),
        vec!["real output".to_string()],
        "no synthetic result over a real one: {synced:?}"
    );
    assert!(matches!(events.last(), Some(AgentEvent::Interrupted(..))));
}

// PRD reliability metric: 0 corrupted sessions out of 50 interruptions
// during a tool dispatch. CORE version: the cancellation point, the number
// of concurrent calls and whether the tools terminate vary on every
// iteration. Replaying the same sweep at CLI level belongs to US-007.
#[tokio::test]
async fn fifty_interruptions_during_dispatch_never_corrupt_the_transcript() {
    const IDS: [&str; 3] = ["c1", "c2", "c3"];
    for run in 0..50usize {
        let ids = &IDS[..=(run % 3)];
        let cancel = CancellationToken::new();
        let h = harness(vec![tool_turn_n(ids)], false, 100_000);
        let mut deps = h.deps.clone();
        deps.cancel = cancel.clone();
        deps.tools = Arc::new(DelayedCancelTools {
            cancel,
            yields: run % 4,
            finish: run % 2 == 0,
        });
        let events = drive(AgentContext::new("mock").push(Message::user("go")), deps).await;

        assert!(
            matches!(events.last(), Some(AgentEvent::Interrupted(..))),
            "run {run}: the loop must stop on its own: {events:?}"
        );
        let synced = h.boundaries.synced.lock().unwrap().clone();
        assert!(
            unanswered_tool_calls(&synced).is_empty(),
            "run {run}: orphan call persisted: {synced:?}"
        );
        assert_eq!(
            persisted_tool_results(&synced).len(),
            ids.len(),
            "run {run}: exactly one result per call: {synced:?}"
        );
    }
}

// US-001 AC6: signal received while the loop is already done -> ignored, with
// no panic and no extra event.
#[tokio::test]
async fn cancel_after_completion_is_ignored() {
    let h = harness(vec![text_turn("done")], false, 100_000);
    let mut deps = h.deps.clone();
    let cancel = CancellationToken::new();
    deps.cancel = cancel.clone();
    let events = drive(AgentContext::new("mock").push(Message::user("hi")), deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    cancel.cancel();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Interrupted(..))),
        "{events:?}"
    );
}
