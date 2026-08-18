//! Turn lifecycle: what the loop commits, persists and hands back.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::message::ContentBlock;
use agent_core::message::Message;
use agent_core::provider::ErrorClass;
use agent_core::provider::ProviderError;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use agent_core::run_headless;
use agent_core::transition::ExhaustReason;
use std::sync::Arc;

mod common;

use common::{
    MissingTools, MockTurn, RichTools, StreamingTools, drive, harness, text_turn, tool_turn,
    with_mock_tool,
};

// ───────── tests ─────────

// US-006 AC1/AC3: headless multi-turn conversation, without Ratatui.
#[tokio::test]
async fn multi_turn_headless_runs_without_tui() {
    let h = harness(vec![tool_turn("c1"), text_turn("fini")], false, 100_000);
    let ctx = with_mock_tool(AgentContext::new("mock").push(Message::user("fais un ls")));
    let res = run_headless(ctx, h.deps).await;
    assert!(res.text.contains("fini"));
    assert!(matches!(res.ended, agent_core::HeadlessEnd::EndTurn));
}

// US-006 AC2: the message is persisted (sync) BEFORE the 1st API call.
#[tokio::test]
async fn transcript_synced_before_stream() {
    let h = harness(vec![text_turn("ok")], false, 100_000);
    let ctx = AgentContext::new("mock").push(Message::user("hello"));
    let _ = run_headless(ctx, h.deps).await;
    let log = h.log.lock().unwrap().clone();
    let sync_at = log.iter().position(|e| *e == "sync");
    let stream_at = log.iter().position(|e| *e == "stream");
    assert!(sync_at.is_some() && stream_at.is_some());
    assert!(sync_at < stream_at, "sync should precede stream: {log:?}");
}

// US-024: the LAST assistant message is synced BEFORE EndTurn, otherwise
// `/resume` loses the last reply. The final sync is delta-only (idempotent):
// `synced.len() == 2` proves the already-synced user message is not duplicated.
#[tokio::test]
async fn final_assistant_turn_synced_before_endturn() {
    let h = harness(vec![text_turn("final answer")], false, 100_000);
    let ctx = AgentContext::new("mock").push(Message::user("question"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

    let synced = h.boundaries.synced.lock().unwrap();
    assert_eq!(
        synced.len(),
        2,
        "user plus final assistant, without duplicate: {synced:?}"
    );
    let last = synced.last().unwrap();
    assert_eq!(last.role, agent_core::message::Role::Assistant);
    assert!(
        last.text().contains("final answer"),
        "the last persisted message should be the final answer: {synced:?}"
    );
}

#[tokio::test]
async fn continuation_commits_assistant_and_resamples_without_user_input() {
    let first = MockTurn::Stream(vec![
        StreamEvent::TextDelta {
            text: "working".into(),
        },
        StreamEvent::Done {
            stop: StopReason::Continue,
        },
    ]);
    let h = harness(vec![first, text_turn("finished")], false, 100_000);
    let ctx = AgentContext::new("mock").push(Message::user("task"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ModelTurn(_)))
            .count(),
        2
    );
    let requests = h.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .iter()
            .filter(|message| message.role == agent_core::message::Role::User)
            .count(),
        1,
        "continuation must not fabricate a user input"
    );
    assert!(requests[1].iter().any(
        |message| message.role == agent_core::message::Role::Assistant
            && message.text().contains("working")
    ));
}

/// A long run is not a runaway. Nothing counts turns any more, so a task that
/// legitimately needs hundreds of them (watching a CI run to completion, which
/// is hours of sleeping and re-checking) reaches its own end.
///
/// The count is deliberately far above the ceiling this loop used to carry: at
/// 50 turns, or at the iteration guard derived from it, this test would stop
/// early and the last event would not be `EndTurn`.
#[tokio::test]
async fn a_run_far_longer_than_the_old_ceiling_is_never_cut_short() {
    const CONTINUATIONS: usize = 400;
    let continuation = || {
        MockTurn::Stream(vec![
            StreamEvent::TextDelta {
                text: "still working".into(),
            },
            StreamEvent::Done {
                stop: StopReason::Continue,
            },
        ])
    };
    let mut turns: Vec<MockTurn> = (0..CONTINUATIONS).map(|_| continuation()).collect();
    turns.push(text_turn("done"));

    let h = harness(turns, false, 100_000);
    let ctx = AgentContext::new("mock").push(Message::user("watch the CI"));
    let events = drive(ctx, h.deps).await;

    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "a long run must end on its own terms: {:?}",
        events.last()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Exhausted(_))),
        "nothing may exhaust a run that is merely long"
    );
    assert_eq!(h.requests.lock().unwrap().len(), CONTINUATIONS + 1);
}

// US-028: context messages (AGENTS.md + env) are prefixed to EVERY
// request but NEVER persisted nor accumulated in the transcript (reloaded).
#[tokio::test]
async fn context_messages_injected_per_request_never_persisted() {
    let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 100_000);
    let ctx = AgentContext::new("mock")
        .with_context_messages(vec![
            Message::user("# AGENTS.md instructions\nCTX_AGENTS"),
            Message::user("<environment>CTX_ENV</environment>"),
        ])
        .push(Message::user("do X"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

    // 1. Every request sent to the provider starts with the 2 context messages.
    let reqs = h.requests.lock().unwrap();
    assert!(reqs.len() >= 2, "at least 2 turns");
    for (i, msgs) in reqs.iter().enumerate() {
        assert!(
            msgs[0].text().contains("CTX_AGENTS") && msgs[1].text().contains("CTX_ENV"),
            "turn {i}: context should prefix the request"
        );
        assert!(
            msgs.iter()
                .filter(|m| m.text().contains("CTX_AGENTS"))
                .count()
                == 1,
            "turn {i}: no context accumulation, one occurrence only"
        );
    }

    // 2. The persisted transcript does NOT contain the context messages.
    let synced = h.boundaries.synced.lock().unwrap();
    assert!(
        !synced
            .iter()
            .any(|m| m.text().contains("CTX_AGENTS") || m.text().contains("CTX_ENV")),
        "ephemeral context should never be persisted: {synced:?}"
    );
}

#[tokio::test]
async fn ephemeral_messages_suffix_request_never_persisted() {
    let h = harness(vec![text_turn("done")], false, 100_000);
    let ctx = AgentContext::new("mock")
        .with_context_messages(vec![Message::user("CTX")])
        .with_ephemeral_messages(vec![Message::user("CONTROL")])
        .push(Message::user("human"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

    let reqs = h.requests.lock().unwrap();
    let first = reqs.first().expect("provider request");
    assert_eq!(first[0].text(), "CTX");
    assert_eq!(first[first.len() - 2].text(), "human");
    assert_eq!(first[first.len() - 1].text(), "CONTROL");

    let synced = h.boundaries.synced.lock().unwrap();
    assert!(synced.iter().any(|m| m.text() == "human"));
    assert!(!synced.iter().any(|m| m.text() == "CONTROL"));
}

#[tokio::test]
async fn stream_without_terminal_fails_closed() {
    let h = harness(
        vec![MockTurn::Stream(vec![StreamEvent::TextDelta {
            text: "partiel".into(),
        }])],
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock").push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::StreamReset)),
        "visible deltas should be removed: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Error(agent_core::AgentError::Provider(_)))
        ),
        "missing terminal event should fail closed: {events:?}"
    );
}

#[tokio::test]
async fn retry_after_visible_delta_resets_headless_output() {
    let h = harness(
        vec![
            MockTurn::StreamThenErr(
                vec![StreamEvent::TextDelta {
                    text: "ghost ".into(),
                }],
                ProviderError::Stream("reset".into()),
            ),
            text_turn("final"),
        ],
        false,
        100_000,
    );
    let mut ctx = AgentContext::new("mock").push(Message::user("go"));
    ctx.turn_id = Some("turn_retry".into());
    let events = drive(ctx, h.deps).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::StreamReset))
            .count(),
        1
    );
    let retry = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::RetryScheduled(view) => Some(view),
            _ => None,
        })
        .expect("retry event");
    assert_eq!(retry.turn_id.as_deref(), Some("turn_retry"));
    assert_eq!(retry.step, 1);
    assert_eq!(retry.ordinal, 2);
    assert_eq!(retry.cause, ErrorClass::Retryable);
    assert_eq!(retry.prompt_fingerprint.len(), 64);
    assert_eq!(retry.model_runtime_fingerprint.len(), 64);
    assert_eq!(retry.tool_plan_fingerprint.len(), 64);
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
}

#[tokio::test]
async fn tool_output_deltas_do_not_change_headless_text() {
    // US-015 AC5: headless mode ignores fragments, its textual output stays
    // byte-for-byte identical to the one produced without streaming.
    let turns = || vec![tool_turn("t1"), text_turn("resultat final")];
    let plain = harness(turns(), false, 100_000);
    let plain_res = run_headless(
        with_mock_tool(AgentContext::new("mock").push(Message::user("go"))),
        plain.deps,
    )
    .await;

    let mut streamed = harness(turns(), false, 100_000);
    streamed.deps.tools = Arc::new(StreamingTools);
    let streamed_res = run_headless(
        with_mock_tool(AgentContext::new("mock").push(Message::user("go"))),
        streamed.deps,
    )
    .await;

    assert_eq!(streamed_res.text, plain_res.text);
    assert_eq!(streamed_res.text, "resultat final");
    // The fragments do exist in the event flow, they are simply not
    // rendered by text mode.
    assert!(streamed_res.events > plain_res.events);
}

#[tokio::test]
async fn maxtokens_plain_text_is_exhausted_not_success() {
    let h = harness(
        vec![MockTurn::Stream(vec![
            StreamEvent::TextDelta {
                text: "truncated".into(),
            },
            StreamEvent::Done {
                stop: StopReason::MaxTokens,
            },
        ])],
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock").push(Message::user("go"));
    let res = run_headless(ctx, h.deps).await;
    assert_eq!(res.text, "");
    assert!(matches!(
        res.ended,
        agent_core::HeadlessEnd::Exhausted(ExhaustReason::MaxOutputTokens {
            visible_output: true
        })
    ));
}

#[tokio::test]
async fn dispatcher_missing_outcome_is_contract_error() {
    let mut h = harness(vec![tool_turn("c1")], false, 100_000);
    h.deps.tools = Arc::new(MissingTools);
    let ctx = AgentContext::new("mock").push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Error(agent_core::AgentError::Provider(_)))
        ),
        "missing outcome should break the contract: {events:?}"
    );
}

/// US-011 AC1: an image read by a tool enters the transcript and is
/// therefore sent to the provider on the next round-trip. It rides INSIDE
/// the tool result: a separate user message would insert a turn the user
/// never took between two tool turns.
/// US-009 AC3: the plan reaches the client as an `AgentEvent`.
#[tokio::test]
async fn tool_images_reach_the_provider_and_the_plan_reaches_the_client() {
    let h = harness(vec![tool_turn("call-1"), text_turn("done")], false, 100_000);
    let mut deps = h.deps.clone();
    deps.tools = Arc::new(RichTools);
    let events = drive(AgentContext::new("mock").push(Message::user("look")), deps).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Plan(view) if view.steps.len() == 1)),
        "the plan must be surfaced to the client: {events:?}"
    );

    let reqs = h.requests.lock().unwrap();
    let second = reqs.get(1).expect("a second round-trip is expected");
    let image = second
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::ToolResult { images, .. } => images.first(),
            _ => None,
        })
        .expect("the image must have entered the transcript");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.data, "QUJD");
    assert!(
        !second
            .iter()
            .any(|m| matches!(m.role, agent_core::Role::User)
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))),
        "no phantom user message carries the image: {second:?}"
    );
}
