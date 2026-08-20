//! Deterministic guardrails: tool loop detection and usage kill-switches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::RunConfig;
use agent_core::compaction::CompactKind;
use agent_core::message::Message;
use agent_core::provider::ProviderError;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use agent_core::provider::TokenUsage;
use agent_core::provider::ToolSpec;
use agent_core::transition::ExhaustReason;
use std::sync::Arc;

mod common;

use common::{
    MockTurn, drive, freeform_tool_turn, harness, harness_with_summary_usage, has_compacted,
    text_turn, tool_turn, tool_turn_n, tool_turn_usage,
};

// US-014 AC1 / US-061: same tool + same args repeated -> the ladder of
// LOOP_GUARD_THRESHOLDS. Executed twice, gentle reminder at 3 and 4, detailed
// reminder at 5, 6 and 7, deterministic stop at 8. The batch is executed at no
// tier, so the extra cost of the ladder is in model round-trips, never effects.
#[tokio::test]
async fn the_loop_guardrail_gives_two_reminders_before_it_stops_the_turn() {
    // The model keeps asking for the same `bash {cmd:ls}` forever.
    let h = harness((0..9).map(|_| tool_turn("c1")).collect(), false, 100_000);
    let ctx = AgentContext::new("mock").push(Message::user("boucle"));
    let events = drive(ctx, h.deps).await;

    let reminders: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult(v) if v.is_error => Some(v.content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reminders.len(), 5, "3, 4, 5, 6 and 7: {events:?}");
    for gentle in &reminders[..2] {
        assert!(gentle.starts_with("Loop guard:"), "{gentle}");
        assert!(
            !gentle.contains("bash") && !gentle.contains("ls"),
            "the gentle register quotes nothing: {gentle}"
        );
    }
    for detailed in &reminders[2..] {
        assert!(
            detailed.contains("bash") && detailed.contains("ls"),
            "the detailed register names the tool and the arguments: {detailed}"
        );
    }

    // NFR-02: the ladder costs round-trips, not effects. Only the two batches
    // below the first tier were ever dispatched.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        2,
        "no tier executes the offending batch: {events:?}"
    );

    // Deterministic stop at the last tier, carrying the real count.
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { count: 8 }))
        ),
        "ToolLoop {{ count: 8 }} ending expected: {events:?}"
    );
}

/// US-061 unhappy path + invariant 11: a batch of three calls dying at the last
/// tier produces exactly ONE terminal state, not one per `tool_use`.
#[tokio::test]
async fn a_multi_call_batch_at_the_last_tier_produces_a_single_terminal_state() {
    let h = harness(
        (0..9).map(|_| tool_turn_n(&["a", "b", "c"])).collect(),
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock").push(Message::user("boucle"));
    let events = drive(ctx, h.deps).await;

    let terminal = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Exhausted(_) | AgentEvent::EndTurn | AgentEvent::Error(_)
            )
        })
        .count();
    assert_eq!(terminal, 1, "exactly one terminal state: {events:?}");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { count: 8 }))
    ));
    // Every signalled batch still answered each of its three `tool_use`.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolResult(v) if v.is_error))
            .count(),
        15,
        "one tool_result per tool_use, on each of the five signalled batches: {events:?}"
    );
}

// US-014: a DIFFERENT batch on every turn does not trip the guardrail.
#[tokio::test]
async fn loop_guardrail_does_not_false_positive_on_distinct_calls() {
    let distinct = |id: &str, cmd: &str| {
        MockTurn::Stream(vec![
            StreamEvent::tool_call_start(id, "bash"),
            StreamEvent::ToolCallDelta {
                id: id.into(),
                input_delta: format!("{{\"cmd\":\"{cmd}\"}}"),
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ])
    };
    let h = harness(
        vec![
            distinct("a", "ls"),
            distinct("b", "pwd"),
            distinct("c", "whoami"),
            text_turn("fini"),
        ],
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock").push(Message::user("three distinct actions"));
    let events = drive(ctx, h.deps).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Exhausted(ExhaustReason::ToolLoop { .. }))),
        "no loop should be detected: {events:?}"
    );
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
}

#[tokio::test]
async fn loop_guardrail_allows_repeated_code_mode_cells() {
    let h = harness(
        vec![
            freeform_tool_turn("cell-1", "exec", "text('tick')"),
            freeform_tool_turn("cell-2", "exec", "text('tick')"),
            freeform_tool_turn("cell-3", "exec", "text('tick')"),
            freeform_tool_turn("cell-4", "exec", "text('tick')"),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    let mut ctx = AgentContext::new("mock").push(Message::user("poll until done"));
    ctx.tools
        .push(ToolSpec::freeform("exec", "code mode", None));
    let events = drive(ctx, h.deps).await;

    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult(result) if result.content.starts_with("Loop guard:")
        )),
        "Code Mode cells are orchestration, not a semantic loop: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "expected normal end: {events:?}"
    );
}

// US-014 AC2: cumulated token budget reached -> kill-switch (edge case #3).
#[tokio::test]
async fn token_budget_kill_switch_stops_run() {
    // Turn 1 consumes 150 tokens (>120); turn 2 must never start.
    let h = harness(
        vec![tool_turn_usage("c1", 100, 50), text_turn("never reached")],
        false,
        1_000_000,
    );
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            token_budget: Some(120),
            max_output_tokens: 10, // small, so the pre-turn estimate does not stop turn 1
            ..RunConfig::default()
        })
        .push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                spent: 150,
                limit: 120
            }))
        ),
        "budget kill-switch expected: {events:?}"
    );
    // The 1st tool turn did happen before the kill-switch.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult(_)))
    );
}

#[tokio::test]
async fn failed_stream_usage_counts_before_retry() {
    let h = harness(
        vec![
            MockTurn::StreamThenErr(
                vec![StreamEvent::Usage {
                    usage: TokenUsage::new(100, 50),
                }],
                ProviderError::Stream("reset".into()),
            ),
            text_turn("never reached"),
        ],
        false,
        1_000_000,
    );
    let log = Arc::clone(&h.log);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            token_budget: Some(120),
            max_output_tokens: 10,
            ..RunConfig::default()
        })
        .push(Message::user("context"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                spent: 150,
                limit: 120
            }))
        ),
        "failed stream usage should count: {events:?}"
    );
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        1,
        "retry should be blocked by the budget before reopening a stream"
    );
}

#[tokio::test]
async fn compaction_usage_counts_against_token_budget() {
    let h = harness_with_summary_usage(
        vec![
            MockTurn::Err(ProviderError::ContextLengthExceeded),
            text_turn("never reached"),
        ],
        false,
        100_000,
        TokenUsage::new(100, 50),
    );
    let log = Arc::clone(&h.log);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            token_budget: Some(120),
            max_output_tokens: 10,
            ..RunConfig::default()
        })
        .push(Message::user("context"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Reactive),
        "reactive compaction expected: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                spent: 150,
                limit: 120
            }))
        ),
        "compaction usage should count: {events:?}"
    );
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        1,
        "no post-compaction stream should start after the budget is reached"
    );
}

// US-014 AC3: PRE-turn estimation -> we stop BEFORE a turn that costs too much
// (no provider call emitted).
#[tokio::test]
async fn pre_turn_estimate_stops_before_expensive_turn() {
    let h = harness(vec![text_turn("never")], false, 1_000_000);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            token_budget: Some(5), // < max_output -> the projection overshoots right away
            max_output_tokens: 100,
            ..RunConfig::default()
        })
        .push(Message::user("task"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.first(),
            Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget { .. }))
        ),
        "pre-turn stop expected: {events:?}"
    );
    // No provider stream must have been opened.
    assert!(
        !h.log.lock().unwrap().contains(&"stream"),
        "provider should not be called: {:?}",
        h.log.lock().unwrap()
    );
}
