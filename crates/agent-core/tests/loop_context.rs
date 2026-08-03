//! Context geometry: prompt baseline, budget, compaction and recovery.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::RunConfig;
use agent_core::compaction::CompactKind;
use agent_core::message::Message;
use agent_core::provider::ErrorClass;
use agent_core::provider::ProviderError;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use agent_core::provider::TokenUsage;
use std::sync::Arc;

mod common;

use common::{
    MockTurn, baseline, drive, harness, has_compacted, resolved_runtime, text_turn, tool_turn,
};

#[tokio::test]
async fn failed_context_transition_prevents_provider_open() {
    let h = harness(vec![text_turn("must not run")], false, 100_000);
    *h.boundaries.fail_context_transition.lock().unwrap() = true;
    let events = drive(
        AgentContext::new("mock").push(Message::user("question")),
        h.deps,
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Session(detail)))
            if detail.contains("context transition")
    ));
    assert!(h.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn runtime_descriptor_gates_reasoning_replay_for_the_turn() {
    let enabled = harness(
        vec![
            MockTurn::Err(ProviderError::Http {
                status: 400,
                message: "encrypted_reasoning is not supported".into(),
                retry_after_ms: None,
            }),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    let mut enabled_ctx = AgentContext::new("ignored").push(Message::user("task"));
    enabled_ctx.model_runtime = Some(resolved_runtime(
        "enabled",
        'a',
        "same",
        agent_core::model::ReasoningReplaySupport::Enabled,
    ));
    let events = drive(enabled_ctx, enabled.deps).await;
    assert_eq!(*enabled.request_replays.lock().unwrap(), vec![true, false]);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ReasoningReplayDisabled { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled(view)
            if view.ordinal == 2
                && view.cause == ErrorClass::ReasoningReplayRejected
                && view.delay_ms == 0
    )));

    let disabled = harness(vec![text_turn("done")], false, 100_000);
    let mut disabled_ctx = AgentContext::new("ignored").push(Message::user("task"));
    disabled_ctx.model_runtime = Some(resolved_runtime(
        "disabled",
        'b',
        "same",
        agent_core::model::ReasoningReplaySupport::Disabled,
    ));
    let _ = drive(disabled_ctx, disabled.deps).await;
    assert_eq!(*disabled.request_replays.lock().unwrap(), vec![false]);
}

#[tokio::test]
async fn comp_hash_change_compacts_with_old_model_before_new_sampling() {
    let old = resolved_runtime(
        "old-model",
        'a',
        "old-hash",
        agent_core::model::ReasoningReplaySupport::Enabled,
    );
    let new = resolved_runtime(
        "new-model",
        'b',
        "new-hash",
        agent_core::model::ReasoningReplaySupport::Enabled,
    );
    let h = harness(vec![text_turn("done")], false, 100_000);
    *h.boundaries.baseline.lock().unwrap() = Some(baseline(&old));
    let mut ctx = AgentContext::new("ignored")
        .push(Message::user("first"))
        .push(Message::assistant_text("answer"))
        .push(Message::user("next"));
    ctx.model_runtime = Some(new.clone());

    let events = drive(ctx, h.deps).await;
    assert!(has_compacted(&events, CompactKind::Auto));
    assert_eq!(*h.complete_models.lock().unwrap(), vec!["old-model"]);
    assert_eq!(*h.request_models.lock().unwrap(), vec!["new-model"]);
    let transitions = h.boundaries.context_transitions.lock().unwrap();
    let switch = transitions.last().expect("new baseline persisted");
    assert_eq!(
        switch
            .from
            .as_ref()
            .map(|from| from.profile_fingerprint.as_str()),
        Some(old.fingerprint.as_str())
    );
    assert_eq!(switch.to.profile_fingerprint, new.fingerprint);
    assert!(
        switch
            .causes
            .contains(&agent_core::ContextTransitionCause::CompHashChanged)
    );
}

#[tokio::test]
async fn overload_fallback_installs_the_resolved_contract() {
    let primary = resolved_runtime(
        "primary",
        'a',
        "compatible",
        agent_core::model::ReasoningReplaySupport::Enabled,
    );
    let mut fallback = resolved_runtime(
        "fallback",
        'b',
        "compatible",
        agent_core::model::ReasoningReplaySupport::Enabled,
    );
    fallback.supports_parallel_tool_calls = true;
    fallback.context_window = 50_000;
    fallback.auto_compact_token_limit = 40_000;
    fallback.fingerprint = "c".repeat(64);
    let h = harness(
        vec![
            MockTurn::Stream(vec![
                StreamEvent::ReasoningReplayDisabled {
                    reason: "rejected once".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::Continue,
                },
            ]),
            MockTurn::Err(ProviderError::Http {
                status: 529,
                message: "overloaded".into(),
                retry_after_ms: None,
            }),
            text_turn("fallback answer"),
        ],
        false,
        100_000,
    );
    let mut ctx = AgentContext::new("ignored")
        .push(Message::user("task"))
        .with_config(RunConfig {
            overload_fallback_model: Some("fallback".into()),
            overload_fallback_runtime: Some(fallback.clone()),
            ..RunConfig::default()
        });
    ctx.model_runtime = Some(primary);

    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    assert_eq!(
        *h.request_models.lock().unwrap(),
        vec!["primary", "primary", "fallback"]
    );
    assert_eq!(
        *h.request_replays.lock().unwrap(),
        vec![true, false, false],
        "a replay downgrade is latched for the whole turn, including fallback"
    );
    assert!(
        h.requests.lock().unwrap()[2]
            .iter()
            .any(|message| message.text().contains("<model_switch"))
    );
    assert_eq!(
        h.boundaries
            .context_transitions
            .lock()
            .unwrap()
            .last()
            .map(|transition| transition.to.profile_fingerprint.clone()),
        Some(fallback.fingerprint)
    );
}

// US-006 AC4 + US-008 AC4: context error -> withholding -> REACTIVE
// compaction, no premature termination, the conversation goes on.
#[tokio::test]
async fn context_error_triggers_withholding_and_reactive_compaction() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::ContextLengthExceeded),
            text_turn("resumed after compaction"),
        ],
        false,
        100_000,
    );
    // real history (>= 2 messages) -> compaction has something to summarize.
    let ctx = AgentContext::new("mock")
        .push(Message::user("initial context"))
        .push(Message::assistant_text("compris"))
        .push(Message::user("long task"));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Reactive),
        "reactive compaction expected: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("resumed"))),
        "conversation should continue after recovery"
    );
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    assert!(
        h.boundaries
            .boundaries
            .lock()
            .unwrap()
            .contains(&CompactKind::Reactive)
    );
}

// US-008 AC2: autocompact threshold crossed -> proactive summary (Compacted::Auto).
#[tokio::test]
async fn autocompaction_triggers_on_budget_threshold() {
    // window 1000, reserve (max_output) 200 -> auto at 640. A large user message
    // (~3000 bytes, roughly 750 heuristic tokens) crosses it at estimation time.
    let huge = "x".repeat(3000);
    let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 1000);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            max_output_tokens: 200,
            ..RunConfig::default()
        })
        .push(Message::user(huge));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Auto),
        "autocompaction expected: {events:?}"
    );
}

// US-007 AC3: provider WITHOUT usage in the stream -> the tokenizer fallback
// feeds the threshold, autocompaction still triggers.
#[tokio::test]
async fn fallback_tokenizer_feeds_threshold_without_usage() {
    let huge = "y".repeat(3000); // ~750 tokens, no Usage emitted by the mock
    let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 1000);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            max_output_tokens: 200,
            ..RunConfig::default()
        })
        .push(Message::user(huge));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Auto),
        "threshold should be fed by the local estimate: {events:?}"
    );
}

// US-008 AC3: repeated autocompact failures -> circuit breaker (no loop).
#[tokio::test]
async fn circuit_breaker_stops_repeated_autocompact_failures() {
    let huge = "z".repeat(3000);
    let h = harness(
        vec![tool_turn("c1")], // one tool turn, then we loop on autocompact
        true,                  // summary_fails -> full_compact always fails
        1000,
    );
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            max_output_tokens: 200,
            compaction_breaker_limit: 3,
            ..RunConfig::default()
        })
        .push(Message::user(huge));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Error(
                agent_core::AgentError::CompactionCircuitBreaker(_)
            ))
        ),
        "circuit breaker expected at the end: {events:?}"
    );
}

// US-006 AC4 (unhappy): if reactive compaction FAILS, the context error is
// propagated (ContextUnrecoverable), no premature end before the recovery
// failure is confirmed.
#[tokio::test]
async fn recovery_failure_propagates_context_unrecoverable() {
    let h = harness(
        vec![MockTurn::Err(ProviderError::ContextLengthExceeded)],
        true, // summary_fails -> reactive compaction fails (provider.complete KO)
        100_000,
    );
    // history >= 2 messages: provider.complete IS called (and fails), this
    // is not the "nothing to summarize" guard short-circuiting.
    let ctx = AgentContext::new("mock")
        .push(Message::user("context"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("task"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Error(
                agent_core::AgentError::ContextUnrecoverable(_)
            ))
        ),
        "recovery failure should propagate ContextUnrecoverable: {events:?}"
    );
}

// US-008 AC4 (distinct): a 413 received MID-stream triggers reactive
// compaction (path distinct from the failure at open time).
#[tokio::test]
async fn http_413_midstream_triggers_reactive_compaction() {
    let h = harness(
        vec![
            MockTurn::StreamThenErr(
                vec![StreamEvent::TextDelta {
                    text: "partiel".into(),
                }],
                ProviderError::Http {
                    status: 413,
                    message: "too long".into(),
                    retry_after_ms: None,
                },
            ),
            text_turn("resumed after 413"),
        ],
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock")
        .push(Message::user("context"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("task"));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Reactive),
        "reactive expected: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("resumed")))
    );
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
}

#[tokio::test]
async fn invalid_context_geometry_fails_before_provider_call() {
    let h = harness(vec![text_turn("never")], false, 100);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            max_output_tokens: 100,
            ..RunConfig::default()
        })
        .push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.first(),
            Some(AgentEvent::Error(agent_core::AgentError::InvalidRequest(_)))
        ),
        "invalid context geometry expected: {events:?}"
    );
    assert!(
        !h.log.lock().unwrap().contains(&"stream"),
        "provider should not be called"
    );
}

#[tokio::test]
async fn overload_fallback_rebuilds_context_budget_for_new_model() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Http {
                status: 529,
                message: "overloaded".into(),
                retry_after_ms: None,
            }),
            text_turn("ok"),
        ],
        false,
        100_000,
    );
    let request_models = Arc::clone(&h.request_models);
    let ctx = AgentContext::new("primary")
        .with_config(RunConfig {
            max_output_tokens: 200,
            overload_fallback_model: Some("small-context".into()),
            ..RunConfig::default()
        })
        .push(Message::user("very long history"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("x".repeat(3000)));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Auto),
        "small fallback window should trigger autocompaction: {events:?}"
    );
    assert_eq!(
        *request_models.lock().unwrap(),
        vec!["primary".to_string(), "small-context".to_string()]
    );
}

// US-006/008: MaxTokens in the middle of a tool_call -> withholding (Recover)
// -> reactive, the tool intent is not silently dropped.
#[tokio::test]
async fn maxtokens_midtool_recovers_in_loop() {
    let h = harness(
        vec![
            MockTurn::Stream(vec![
                StreamEvent::tool_call_start("c1", "bash"),
                StreamEvent::ToolCallDelta {
                    id: "c1".into(),
                    input_delta: "{\"cm".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::MaxTokens,
                },
            ]),
            text_turn("regenerated"),
        ],
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock")
        .push(Message::user("initial context"))
        .push(Message::assistant_text("ok"))
        .push(Message::user("do X"));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Reactive),
        "reactive expected: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("regenerated")))
    );
}

// US-008 AC1: microcompaction triggered INSIDE the loop (micro threshold 70%,
// below the auto 80%) -> Compacted(Micro), without Auto.
#[tokio::test]
async fn microcompaction_triggers_in_loop_below_auto() {
    // window 1000, reserve 200 -> micro 560, auto 640. usage=600 ∈ [560,640).
    let turn = MockTurn::Stream(vec![
        StreamEvent::Usage {
            usage: TokenUsage::new(600, 5),
        },
        StreamEvent::tool_call_start("c1", "bash"),
        StreamEvent::ToolCallDelta {
            id: "c1".into(),
            input_delta: "{}".into(),
        },
        StreamEvent::ToolCallEnd { id: "c1".into() },
        StreamEvent::Done {
            stop: StopReason::ToolUse,
        },
    ]);
    let h = harness(vec![turn], false, 1000);
    let ctx = AgentContext::new("mock")
        .with_config(RunConfig {
            max_output_tokens: 200,
            ..RunConfig::default()
        })
        .push(Message::user("go"))
        .push(Message::tool_result("a", "r1", false))
        .push(Message::tool_result("b", "r2", false))
        .push(Message::tool_result("c", "r3", false))
        .push(Message::tool_result("d", "r4", false));
    let events = drive(ctx, h.deps).await;
    assert!(
        has_compacted(&events, CompactKind::Micro),
        "microcompaction expected: {events:?}"
    );
    assert!(
        !has_compacted(&events, CompactKind::Auto),
        "pas d'auto sous le seuil: {events:?}"
    );
}
