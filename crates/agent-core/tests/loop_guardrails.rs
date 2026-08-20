//! Deterministic guardrails: tool loop detection and usage kill-switches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::RunConfig;
use agent_core::compaction::CompactKind;
use agent_core::message::Message;
use agent_core::message::ToolErrorKind;
use agent_core::provider::ProviderError;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use agent_core::provider::TokenUsage;
use agent_core::provider::ToolSpec;
use agent_core::tools::ModelToolResult;
use agent_core::tools::ToolDispatch;
use agent_core::tools::ToolEventSink;
use agent_core::tools::ToolInvocation;
use agent_core::transition::ExhaustReason;
use std::sync::Arc;

mod common;

use common::{
    MockTurn, drive, freeform_tool_turn, harness, harness_with_summary_usage, has_compacted,
    named_tool_turn, text_turn, tool_turn, tool_turn_n, tool_turn_usage,
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

/// The guardrail's own refusals, told apart from any other tool error by the
/// prefix the three registers share.
fn loop_guard_reminders(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult(view) if view.content.starts_with("Loop guard:") => {
                Some(view.content.as_str())
            }
            _ => None,
        })
        .collect()
}

/// The exempt poll of a Code Mode cell, as the step must expose it for the call
/// to reach the pipeline at all.
fn poll_spec() -> ToolSpec {
    ToolSpec::function(
        "wait",
        "poll a code mode cell",
        serde_json::json!({
            "type": "object",
            "properties": { "cell_id": { "type": "string" } },
            "required": ["cell_id"],
            "additionalProperties": false
        }),
    )
}

/// Steering queue scripted by the test: one interjection, delivered at the
/// `nth` safe point the loop reaches, and nothing at any other.
struct ScriptedSteering {
    nth: usize,
    takes: std::sync::atomic::AtomicUsize,
}

impl ScriptedSteering {
    fn at(nth: usize) -> Arc<Self> {
        Arc::new(Self {
            nth,
            takes: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Never delivers anything: the queue the unhappy path drains in vain.
    fn silent() -> Arc<Self> {
        Self::at(usize::MAX)
    }

    fn takes(&self) -> usize {
        self.takes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl agent_core::input::InputQueue for ScriptedSteering {
    fn take(&self) -> Vec<Message> {
        let seen = self.takes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if seen == self.nth {
            vec![Message::user("attends, redemande le même appel")]
        } else {
            Vec::new()
        }
    }

    async fn ready(&self) {
        // The interjection is taken at the safe point, not mid-stream: this
        // test drives the drain, not the sampling cut.
        std::future::pending().await
    }
}

/// US-063 AC3: two identical batches, a human interjection, two identical
/// batches. A repetition on either side of an interjection is a user asking
/// for the call again, so the run breaks and no tier is crossed.
#[tokio::test]
async fn a_steering_input_breaks_the_loop_guardrail_run() {
    let h = harness(
        vec![
            tool_turn("c1"),
            tool_turn("c2"),
            tool_turn("c3"),
            tool_turn("c4"),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    // The third safe point, so the interjection lands between the second and
    // the third identical batch.
    let steering = ScriptedSteering::at(2);
    let ctx = AgentContext::new("mock")
        .with_inputs(Arc::clone(&steering) as Arc<dyn agent_core::input::InputQueue>)
        .push(Message::user("boucle"));
    let events = drive(ctx, h.deps).await;

    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult(result) if result.content.starts_with("Loop guard:")
        )),
        "the interjection breaks the run before any tier: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        4,
        "every batch runs, none is refused: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "expected normal end: {events:?}"
    );
    assert!(steering.takes() >= 3, "the safe point drained the queue");
    let requests = h.requests.lock().unwrap();
    assert!(
        requests.iter().any(|messages| messages
            .iter()
            .any(|message| format!("{message:?}").contains("redemande le même appel"))),
        "the interjection actually entered the transcript"
    );
}

/// US-063 AC4 unhappy path: the same turn, with a queue that stays empty at
/// every pass, crosses the tiers exactly as before. A `take()` returning zero
/// message resets nothing, otherwise steering would disable the guardrail.
#[tokio::test]
async fn an_empty_steering_queue_resets_nothing() {
    let h = harness(
        vec![
            tool_turn("c1"),
            tool_turn("c2"),
            tool_turn("c3"),
            tool_turn("c4"),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    let steering = ScriptedSteering::silent();
    let ctx = AgentContext::new("mock")
        .with_inputs(Arc::clone(&steering) as Arc<dyn agent_core::input::InputQueue>)
        .push(Message::user("boucle"));
    let events = drive(ctx, h.deps).await;

    let reminders = loop_guard_reminders(&events);
    assert_eq!(
        reminders.len(),
        2,
        "the third and fourth occurrences still reach the first tier: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        2,
        "only the two batches below the first tier ran: {events:?}"
    );
    assert!(steering.takes() >= 5, "the queue was drained at every pass");
}

/// US-065 AC4: `bash`, `wait`, `bash`, `wait`, `bash`. The exempt poll is
/// TRANSPARENT, so the three `bash` occurrences are counted and the first tier
/// is reached on the third: a `wait` between two identical calls is part of the
/// loop, not a break in it.
#[tokio::test]
async fn an_exempt_call_between_two_identical_calls_does_not_break_the_run() {
    let h = harness(
        vec![
            tool_turn("c1"),
            named_tool_turn("w1", "wait", "{\"cell_id\":\"cell-1\"}"),
            tool_turn("c2"),
            named_tool_turn("w2", "wait", "{\"cell_id\":\"cell-1\"}"),
            tool_turn("c3"),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    let mut ctx = AgentContext::new("mock").push(Message::user("boucle en polling"));
    ctx.tools.push(poll_spec());
    let events = drive(ctx, h.deps).await;

    let reminders = loop_guard_reminders(&events);
    assert_eq!(
        reminders.len(),
        1,
        "one tier, reached by the third `bash`: {events:?}"
    );
    assert!(
        reminders[0].starts_with("Loop guard:"),
        "{:?}",
        reminders[0]
    );
    // Four calls ran: two `bash` under the tier and the two polls between them.
    // The third `bash` is the only one refused, which is where the count of
    // three lands.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        4,
        "the polls ran, the third `bash` did not: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "expected normal end: {events:?}"
    );
}

/// US-065 AC6 unhappy path: a run of pure `wait` calls crosses no tier and
/// leaves the count unchanged, which the three `bash` calls that follow prove
/// by reaching the first tier exactly on the third, not sooner.
#[tokio::test]
async fn a_run_of_exempt_calls_crosses_no_tier_and_leaves_the_count_unchanged() {
    let mut turns: Vec<MockTurn> = (0..5)
        .map(|i| named_tool_turn(&format!("w{i}"), "wait", "{\"cell_id\":\"cell-1\"}"))
        .collect();
    turns.extend([tool_turn("c1"), tool_turn("c2"), tool_turn("c3")]);
    turns.push(text_turn("done"));
    let h = harness(turns, false, 100_000);
    let mut ctx = AgentContext::new("mock").push(Message::user("poll puis boucle"));
    ctx.tools.push(poll_spec());
    let events = drive(ctx, h.deps).await;

    let reminders = loop_guard_reminders(&events);
    assert_eq!(
        reminders.len(),
        1,
        "no tier during the polls, one on the third `bash`: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        7,
        "five polls and two `bash` ran: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "expected normal end: {events:?}"
    );
}

/// The command `PermissionWall` refuses. Destructive on purpose: this is the
/// shape of call a real permission gate stops.
const DENIED_CMD: &str = "rm -rf /";

fn denied_turn(id: &str) -> MockTurn {
    named_tool_turn(id, "bash", &format!("{{\"cmd\":\"{DENIED_CMD}\"}}"))
}

fn allowed_turn(id: &str) -> MockTurn {
    named_tool_turn(id, "bash", "{\"cmd\":\"pwd\"}")
}

/// Dispatcher standing in for the registry at its permission gate: the
/// destructive call is refused, every other one echoes. The refusal is produced
/// INSIDE the dispatch, which is the only place the guardrail cannot see it.
struct PermissionWall;

#[async_trait::async_trait]
impl ToolDispatch for PermissionWall {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        calls
            .into_iter()
            .map(|call| {
                if call.input.get("cmd").and_then(serde_json::Value::as_str) == Some(DENIED_CMD) {
                    ModelToolResult::rejected(
                        call.id,
                        "the user refused this command",
                        ToolErrorKind::PermissionDenied,
                    )
                } else {
                    ModelToolResult::new(call.id, "ok".into(), false, true, None)
                }
            })
            .collect()
    }
}

/// Results the dispatch refused, told apart from the guardrail's own answers by
/// their error kind.
fn results_of_kind(events: &[AgentEvent], kind: ToolErrorKind) -> usize {
    events
        .iter()
        .filter(
            |event| matches!(event, AgentEvent::ToolResult(view) if view.error_kind == Some(kind)),
        )
        .count()
}

/// US-066 AC1. `observe` runs UPSTREAM of the dispatch, and a permission
/// refusal is produced inside it, so a refused call counts as an ordinary
/// repetition. Move `observe` under the dispatch and this test fails: without
/// it, a model hammering a call the user keeps refusing would never be stopped,
/// which is exactly the loop the guardrail exists to break.
#[tokio::test]
async fn a_call_refused_by_permission_still_counts_up_the_ladder_to_the_stop() {
    let mut h = harness(
        (0..9).map(|i| denied_turn(&format!("d{i}"))).collect(),
        false,
        100_000,
    );
    h.deps.tools = Arc::new(PermissionWall);
    let ctx = AgentContext::new("mock").push(Message::user("insiste sur l'appel refusé"));
    let events = drive(ctx, h.deps).await;

    assert_eq!(
        results_of_kind(&events, ToolErrorKind::PermissionDenied),
        2,
        "the two batches below the first tier reached the gate and were refused: {events:?}"
    );
    assert_eq!(
        loop_guard_reminders(&events).len(),
        5,
        "the refusals crossed the tiers at 3, 4, 5, 6 and 7: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { count: 8 }))
        ),
        "a refused call must reach the deterministic stop like any other: {events:?}"
    );
}

/// US-066 AC3. Same property one step earlier: an unknown name is rejected by
/// the step's catalog before the dispatcher is even touched, and
/// `crates/agent-tools/src/registry.rs` states that such a call is NOT exempt.
/// A model looping on a misspelled tool is therefore stopped like any other.
#[tokio::test]
async fn an_unknown_tool_name_counts_up_the_ladder_because_nothing_exempts_it() {
    let h = harness(
        (0..9)
            .map(|i| named_tool_turn(&format!("u{i}"), "bahs", "{\"cmd\":\"ls\"}"))
            .collect(),
        false,
        100_000,
    );
    let ctx = AgentContext::new("mock").push(Message::user("boucle sur un nom inconnu"));
    let events = drive(ctx, h.deps).await;

    assert_eq!(
        results_of_kind(&events, ToolErrorKind::UnknownTool),
        2,
        "the two batches below the first tier were rejected as unknown: {events:?}"
    );
    assert_eq!(
        loop_guard_reminders(&events).len(),
        5,
        "an unknown name crosses the tiers like a known one: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { count: 8 }))
        ),
        "expected the deterministic stop: {events:?}"
    );
}

/// US-066 unhappy path: a refusal is an ordinary call, not a state of its own.
/// Two refused batches then a different, accepted one, and the count restarts
/// at 1: the accepted call has to be repeated three times of its own to reach
/// the first tier.
#[tokio::test]
async fn an_accepted_call_after_a_refusal_restarts_the_count_at_one() {
    let mut h = harness(
        vec![
            denied_turn("d0"),
            denied_turn("d1"),
            allowed_turn("p1"),
            allowed_turn("p2"),
            allowed_turn("p3"),
            text_turn("done"),
        ],
        false,
        100_000,
    );
    h.deps.tools = Arc::new(PermissionWall);
    let ctx = AgentContext::new("mock").push(Message::user("refus puis autre chose"));
    let events = drive(ctx, h.deps).await;

    assert_eq!(
        results_of_kind(&events, ToolErrorKind::PermissionDenied),
        2,
        "both refusals happened: {events:?}"
    );
    assert_eq!(
        loop_guard_reminders(&events).len(),
        1,
        "one tier, reached by the third accepted call and not sooner: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall(_)))
            .count(),
        4,
        "two refused batches and two accepted ones ran: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "expected normal end: {events:?}"
    );
}
