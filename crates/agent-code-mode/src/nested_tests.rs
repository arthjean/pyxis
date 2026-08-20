//! US-008 proofs of the nested dispatch, without a JavaScript engine: what a
//! cell reaches is the step's own tool pipeline, and nothing else.

use std::collections::HashSet;
use std::sync::Mutex;

use agent_core::message::{ToolCallFormat, ToolErrorKind};
use agent_core::tools::{
    ModelToolResult, ToolDispatch, ToolDispatchEvent, ToolEventSink, ToolInvocation,
};

use super::*;
use crate::protocol::SessionId;

/// Records what reached the pipeline and answers what the test asked for.
#[derive(Default)]
struct RecordingTools {
    seen: Mutex<Vec<(String, String, ToolCallFormat)>>,
    denied: HashSet<String>,
    emit_output: bool,
    /// Calls this dispatcher declares repeatable by design, the way the real
    /// registry answers for a terminal poll or a Code Mode cell.
    repeatable: HashSet<String>,
}

impl RecordingTools {
    fn seen(&self) -> Vec<(String, String, ToolCallFormat)> {
        match self.seen.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[async_trait::async_trait]
impl ToolDispatch for RecordingTools {
    fn loop_guard_exempt(&self, call: &ToolInvocation) -> bool {
        self.repeatable.contains(&call.name)
    }

    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        let mut outcomes = Vec::with_capacity(calls.len());
        for call in calls {
            if self.emit_output {
                events.emit(ToolDispatchEvent::OutputDelta {
                    id: call.id.clone(),
                    stream: agent_core::event::OutputStream::Stdout,
                    chunk: b"progress".to_vec(),
                });
            }
            let rendered = match call.format {
                ToolCallFormat::Text => call.input.as_str().unwrap_or_default().to_string(),
                ToolCallFormat::Json => call.input.to_string(),
            };
            match self.seen.lock() {
                Ok(mut guard) => guard.push((call.id.clone(), rendered.clone(), call.format)),
                Err(poisoned) => {
                    poisoned
                        .into_inner()
                        .push((call.id.clone(), rendered.clone(), call.format))
                }
            }
            if self.denied.contains(&call.name) {
                outcomes.push(ModelToolResult::rejected(
                    call.id,
                    "refused by the permission policy",
                    ToolErrorKind::PermissionDenied,
                ));
            } else {
                outcomes.push(ModelToolResult::new(
                    call.id,
                    format!("ran {} on {rendered}", call.name),
                    false,
                    false,
                    None,
                ));
            }
        }
        outcomes
    }
}

fn function_spec(name: &str) -> ToolSpec {
    ToolSpec::function(
        name,
        "test tool",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["value"],
            "properties": { "value": { "type": "string" } }
        }),
    )
}

fn snapshot(tools: Arc<RecordingTools>, specs: Vec<ToolSpec>) -> ToolDispatchSnapshot {
    ToolDispatchSnapshot::new(7, specs, tools)
}

/// `PlanDispatcher::dispatch` blocks, exactly like a cell thread does, so the
/// tests drive it from a plain OS thread rather than from a Tokio worker.
fn dispatch_off_runtime(
    dispatcher: Arc<PlanDispatcher>,
    calls: Vec<NestedToolCall>,
) -> Vec<NestedToolOutcome> {
    std::thread::spawn(move || {
        calls
            .into_iter()
            .map(|call| dispatcher.dispatch(call))
            .collect()
    })
    .join()
    .expect("the dispatch thread must not panic")
}

fn call(tool: &str, input: NestedToolInput) -> NestedToolCall {
    call_in_cell(0, tool, input)
}

fn call_in_cell(ordinal: u64, tool: &str, input: NestedToolInput) -> NestedToolCall {
    NestedToolCall {
        cell_id: CellId::new(&SessionId::new("thread-a"), ordinal),
        call_id: format!("nested-{tool}"),
        tool: tool.to_string(),
        input,
    }
}

fn dispatcher(tools: Arc<RecordingTools>, specs: Vec<ToolSpec>) -> Arc<PlanDispatcher> {
    dispatcher_with_guard(tools, specs, NestedLoopGuard::default())
}

fn dispatcher_with_guard(
    tools: Arc<RecordingTools>,
    specs: Vec<ToolSpec>,
    loop_guard: NestedLoopGuard,
) -> Arc<PlanDispatcher> {
    Arc::new(PlanDispatcher::new(
        &snapshot(tools, specs),
        &[crate::tools::EXEC_TOOL_NAME, crate::tools::WAIT_TOOL_NAME],
        loop_guard,
        ToolEventSink::default(),
        tokio::runtime::Handle::current(),
    ))
}

#[tokio::test]
async fn a_nested_call_reaches_the_same_pipeline_a_direct_call_would() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);

    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({ "value": "x" })),
        )],
    );
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].is_error, "{:?}", outcomes[0]);
    assert_eq!(
        outcomes[0].error_kind, None,
        "a category on a success would contradict `is_error`"
    );
    assert!(outcomes[0].content.contains("ran read"));

    let seen = tools.seen();
    assert_eq!(seen.len(), 1, "the call went through the pipeline once");
    assert_eq!(seen[0].2, ToolCallFormat::Json);
    assert!(
        seen[0].0.starts_with("thread-a#0::nested-read::"),
        "the invocation id correlates the cell and the call: {}",
        seen[0].0
    );
}

#[tokio::test]
#[allow(
    clippy::panic,
    reason = "an unexpected event pair has no assertion to make"
)]
async fn a_nested_call_publishes_its_native_client_lifecycle() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    dispatcher.bind_events(ToolEventSink::new(event_tx));

    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({ "value": "x" })),
        )],
    );
    assert!(!outcomes[0].is_error);

    let events: Vec<ToolDispatchEvent> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(events.len(), 2, "one start and one terminal event");
    let (started_id, finished_id) = match (&events[0], &events[1]) {
        (
            ToolDispatchEvent::NestedToolCall(started),
            ToolDispatchEvent::NestedToolResult(finished),
        ) => {
            assert_eq!(started.name, "read");
            assert!(!finished.is_error);
            (&started.id, &finished.id)
        }
        unexpected => panic!("unexpected nested lifecycle: {unexpected:?}"),
    };
    assert_eq!(started_id, finished_id);
}

#[tokio::test]
async fn nested_events_survive_the_gap_between_exec_and_wait() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);
    let (stale_tx, stale_rx) = tokio::sync::mpsc::unbounded_channel();
    dispatcher.bind_events(ToolEventSink::new(stale_tx));
    drop(stale_rx);

    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({ "value": "late" })),
        )],
    );
    assert!(!outcomes[0].is_error);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    dispatcher.bind_events(ToolEventSink::new(event_tx));
    let events: Vec<ToolDispatchEvent> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(events.len(), 2, "pending lifecycle must flush on wait");
}

#[tokio::test]
async fn nested_pipeline_events_use_the_same_buffered_bridge() {
    let tools = Arc::new(RecordingTools {
        emit_output: true,
        ..RecordingTools::default()
    });
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);
    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({ "value": "late" })),
        )],
    );
    assert!(!outcomes[0].is_error);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    dispatcher.bind_events(ToolEventSink::new(event_tx));
    let events: Vec<ToolDispatchEvent> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert!(matches!(events[0], ToolDispatchEvent::NestedToolCall(_)));
    assert!(matches!(events[1], ToolDispatchEvent::OutputDelta { .. }));
    assert!(matches!(events[2], ToolDispatchEvent::NestedToolResult(_)));
}

#[tokio::test]
async fn a_freeform_nested_tool_keeps_a_text_input() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(
        Arc::clone(&tools),
        vec![ToolSpec::freeform("apply_patch", "patches", None)],
    );
    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "apply_patch",
            NestedToolInput::Text("*** Begin Patch".into()),
        )],
    );
    assert!(!outcomes[0].is_error);
    let seen = tools.seen();
    assert_eq!(seen[0].2, ToolCallFormat::Text);
    assert_eq!(seen[0].1, "*** Begin Patch");
}

/// US-008 AC4: a name the cell invented never becomes an effect.
#[tokio::test]
async fn an_unknown_tool_is_refused_before_the_pipeline() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);
    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call("rm_rf", NestedToolInput::Json(serde_json::json!({})))],
    );
    assert!(outcomes[0].is_error);
    assert_eq!(outcomes[0].error_kind, Some("unknown_tool"));
    assert!(tools.seen().is_empty(), "nothing reached the pipeline");
}

/// US-008 AC4: a refusal comes back categorized, not as free text.
#[tokio::test]
async fn a_denied_tool_comes_back_as_a_typed_refusal() {
    let tools = Arc::new(RecordingTools {
        denied: HashSet::from(["write".to_string()]),
        ..RecordingTools::default()
    });
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("write")]);
    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call(
            "write",
            NestedToolInput::Json(serde_json::json!({ "value": "x" })),
        )],
    );
    assert!(outcomes[0].is_error);
    assert_eq!(outcomes[0].error_kind, Some("permission_denied"));
}

/// US-009 AC1 seen from here: `exec` and `wait` are model-facing only. A cell
/// calling them would recurse into itself or drive its own session.
#[tokio::test]
async fn the_code_mode_tools_are_not_callable_from_inside_a_cell() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(
        Arc::clone(&tools),
        vec![
            crate::tools::exec_tool_spec(&[], true),
            crate::tools::wait_tool_spec(),
            function_spec("read"),
        ],
    );
    let catalog: Vec<String> = dispatcher
        .catalog()
        .iter()
        .map(|spec| spec.name.clone())
        .collect();
    assert_eq!(catalog, vec!["read".to_string()]);

    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![call("exec", NestedToolInput::Text("text(1)".into()))],
    );
    assert_eq!(outcomes[0].error_kind, Some("unknown_tool"));
    assert!(tools.seen().is_empty());
}

/// US-008 AC3: several calls stay correlated and end in the order they were
/// made.
#[tokio::test]
async fn several_nested_calls_stay_correlated_and_ordered() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(
        Arc::clone(&tools),
        vec![function_spec("first"), function_spec("second")],
    );
    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        vec![
            call(
                "first",
                NestedToolInput::Json(serde_json::json!({"value": "1"})),
            ),
            call(
                "second",
                NestedToolInput::Json(serde_json::json!({"value": "2"})),
            ),
            call(
                "first",
                NestedToolInput::Json(serde_json::json!({"value": "3"})),
            ),
        ],
    );
    let answered: Vec<&str> = outcomes
        .iter()
        .map(|outcome| outcome.tool.as_str())
        .collect();
    assert_eq!(answered, vec!["first", "second", "first"]);
    let seen: Vec<String> = tools.seen().into_iter().map(|entry| entry.1).collect();
    assert_eq!(
        seen,
        vec![
            "{\"value\":\"1\"}".to_string(),
            "{\"value\":\"2\"}".to_string(),
            "{\"value\":\"3\"}".to_string()
        ]
    );
    // Two calls to the same tool never share an invocation identifier.
    let ids: HashSet<String> = tools.seen().into_iter().map(|entry| entry.0).collect();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn repeated_nested_effects_are_guarded_across_outer_cells() {
    let tools = Arc::new(RecordingTools::default());
    let loop_guard = NestedLoopGuard::default();
    let mut outcomes = Vec::new();

    for ordinal in 0..3 {
        let dispatcher = dispatcher_with_guard(
            Arc::clone(&tools),
            vec![function_spec("write")],
            loop_guard.clone(),
        );
        outcomes.extend(dispatch_off_runtime(
            dispatcher,
            vec![call_in_cell(
                ordinal,
                "write",
                NestedToolInput::Json(serde_json::json!({"value": "same"})),
            )],
        ));
        loop_guard.finish_cell(&CellId::new(&SessionId::new("thread-a"), ordinal));
    }

    assert!(!outcomes[0].is_error);
    assert!(!outcomes[1].is_error);
    assert_eq!(outcomes[2].error_kind, Some("semantic"));
    assert!(
        outcomes[2].content.starts_with("Loop guard:"),
        "the first tier answers in the gentle register: {}",
        outcomes[2].content
    );
    assert!(
        !outcomes[2].content.contains("same"),
        "the gentle register quotes no argument: {}",
        outcomes[2].content
    );
    assert_eq!(tools.seen().len(), 2, "the third effect never executes");
}

/// US-062. The nested site carries the same ladder as the outer one, and its
/// last tier LOCKS: a cell is JavaScript, so it can catch an error and call
/// again, which is precisely the loop the guard exists to break.
#[tokio::test]
async fn the_nested_ladder_escalates_then_locks_the_dispatch_for_the_turn() {
    let tools = Arc::new(RecordingTools::default());
    let loop_guard = NestedLoopGuard::default();
    let specs = vec![function_spec("write"), function_spec("read")];
    let dispatcher = dispatcher_with_guard(Arc::clone(&tools), specs.clone(), loop_guard.clone());
    let same = || {
        call(
            "write",
            NestedToolInput::Json(serde_json::json!({"value": "same"})),
        )
    };

    let outcomes = dispatch_off_runtime(
        Arc::clone(&dispatcher),
        (0..8).map(|_| same()).collect::<Vec<_>>(),
    );

    assert!(!outcomes[0].is_error, "1st runs");
    assert!(!outcomes[1].is_error, "2nd runs");
    for gentle in &outcomes[2..4] {
        assert!(!gentle.content.contains("same"), "gentle at 3 and 4");
    }
    for detailed in &outcomes[4..7] {
        assert!(
            detailed.content.contains("write") && detailed.content.contains("\"same\""),
            "detailed at 5, 6 and 7: {}",
            detailed.content
        );
    }
    assert!(
        outcomes[7].content.contains("Dispatch is stopped"),
        "the last tier is terminal: {}",
        outcomes[7].content
    );
    assert_eq!(tools.seen().len(), 2, "only the first two ever executed");

    // Unhappy path: the lock is not scoped to the tool that looped, and a cell
    // ending does not shed it.
    loop_guard.finish_cell(&CellId::new(&SessionId::new("thread-a"), 0));
    let after_lock = dispatch_off_runtime(
        dispatcher_with_guard(Arc::clone(&tools), specs.clone(), loop_guard.clone()),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({"value": "unrelated"})),
        )],
    );
    assert!(after_lock[0].is_error, "a locked turn refuses every tool");
    assert_eq!(
        tools.seen().len(),
        2,
        "no call reaches the dispatcher once the lock is down"
    );

    loop_guard.reset();
    let next_turn = dispatch_off_runtime(
        dispatcher_with_guard(Arc::clone(&tools), specs, loop_guard.clone()),
        vec![call(
            "read",
            NestedToolInput::Json(serde_json::json!({"value": "unrelated"})),
        )],
    );
    assert!(!next_turn[0].is_error, "a new turn starts free of the lock");
    assert_eq!(tools.seen().len(), 3);
}

#[tokio::test]
async fn pure_cells_and_turn_boundaries_break_nested_effect_repetition() {
    let tools = Arc::new(RecordingTools::default());
    let loop_guard = NestedLoopGuard::default();

    for ordinal in 0..2 {
        let dispatcher = dispatcher_with_guard(
            Arc::clone(&tools),
            vec![function_spec("write")],
            loop_guard.clone(),
        );
        let outcome = dispatch_off_runtime(
            dispatcher,
            vec![call_in_cell(
                ordinal,
                "write",
                NestedToolInput::Json(serde_json::json!({"value": "same"})),
            )],
        );
        assert!(!outcome[0].is_error);
        loop_guard.finish_cell(&CellId::new(&SessionId::new("thread-a"), ordinal));
    }

    let pure_cell = CellId::new(&SessionId::new("thread-a"), 2);
    loop_guard.finish_cell(&pure_cell);
    let after_pure = dispatcher_with_guard(
        Arc::clone(&tools),
        vec![function_spec("write")],
        loop_guard.clone(),
    );
    let outcome = dispatch_off_runtime(
        after_pure,
        vec![call_in_cell(
            3,
            "write",
            NestedToolInput::Json(serde_json::json!({"value": "same"})),
        )],
    );
    assert!(!outcome[0].is_error, "a pure cell resets the run");

    loop_guard.reset();
    let next_turn = dispatcher_with_guard(
        Arc::clone(&tools),
        vec![function_spec("write")],
        loop_guard.clone(),
    );
    let outcome = dispatch_off_runtime(
        next_turn,
        vec![call_in_cell(
            4,
            "write",
            NestedToolInput::Json(serde_json::json!({"value": "same"})),
        )],
    );
    assert!(!outcome[0].is_error, "a new turn starts a fresh guard");
}

#[tokio::test]
async fn nested_terminal_polls_reset_the_effect_loop_guard() {
    let tools = Arc::new(RecordingTools {
        repeatable: HashSet::from(["write_stdin".to_string()]),
        ..RecordingTools::default()
    });
    let loop_guard = NestedLoopGuard::default();

    for _ in 0..4 {
        let dispatcher = dispatcher_with_guard(
            Arc::clone(&tools),
            vec![function_spec("write_stdin")],
            loop_guard.clone(),
        );
        let outcome = dispatch_off_runtime(
            dispatcher,
            vec![call(
                "write_stdin",
                NestedToolInput::Json(serde_json::json!({
                    "session_id": 7,
                    "chars": "",
                    "terminate": false
                })),
            )],
        );
        assert!(!outcome[0].is_error, "polls must stay repeatable");
    }

    assert_eq!(tools.seen().len(), 4);
}

/// Retention exists to bridge `exec` and a later `wait`, not to buffer a whole
/// run: an unattended dispatcher must not grow with everything the nested calls
/// ever printed.
#[tokio::test]
async fn retained_nested_events_are_capped_instead_of_growing_without_bound() {
    let tools = Arc::new(RecordingTools::default());
    let dispatcher = dispatcher(Arc::clone(&tools), vec![function_spec("read")]);
    // Two events per call, so this overflows the ceiling by a margin. Inputs
    // differ so the effect loop guard has nothing to catch.
    let rounds = super::MAX_PENDING_EVENTS;
    let calls: Vec<NestedToolCall> = (0..rounds)
        .map(|index| NestedToolCall {
            cell_id: CellId::new(&SessionId::new("thread-a"), 0),
            call_id: format!("nested-{index}"),
            tool: "read".to_string(),
            input: NestedToolInput::Json(serde_json::json!({ "value": index.to_string() })),
        })
        .collect();
    let outcomes = dispatch_off_runtime(Arc::clone(&dispatcher), calls);
    assert!(
        outcomes.iter().all(|outcome| !outcome.is_error),
        "distinct inputs must not trip the loop guard"
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    dispatcher.bind_events(ToolEventSink::new(event_tx));
    let events: Vec<ToolDispatchEvent> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(events.len(), super::MAX_PENDING_EVENTS);
    assert!(
        matches!(events[0], ToolDispatchEvent::NestedToolCall(_)),
        "the events kept are the oldest, so the pairs that survive stay whole"
    );
}

#[tokio::test]
async fn a_session_without_a_dispatcher_refuses_every_nested_call() {
    let outcome =
        NoNestedTools.dispatch(call("read", NestedToolInput::Json(serde_json::json!({}))));
    assert!(outcome.is_error);
    assert_eq!(outcome.error_kind, Some("unknown_tool"));
    assert!(NoNestedTools.catalog().is_empty());
}
