//! US-008 proofs at the Registry level: `exec` and `wait` are real tools, the
//! model sees a freeform `exec`, and a cell reaches the pipeline it should.

use std::collections::HashMap;

use agent_code_mode::nested::{NestedToolCall, NestedToolOutcome};
use agent_code_mode::protocol::{CellFailure, CellFailureKind, SessionId, ShutdownReport};
use agent_code_mode::session::{CellEngine, CellSink};
use agent_core::provider::ToolSpec;
use agent_core::tools::ToolInvocation;

use super::*;
use crate::registry::Registry;

/// Engine whose cells are scripted by the test: no V8 in this crate.
#[derive(Default)]
struct ScriptedEngine {
    sinks: Mutex<HashMap<CellId, CellSink>>,
}

impl ScriptedEngine {
    fn sink(&self, cell: &CellId) -> CellSink {
        lock(&self.sinks).get(cell).cloned().expect("cell started")
    }
}

impl CellEngine for ScriptedEngine {
    fn start(&self, cell: CellId, _request: &ExecuteRequest, sink: CellSink) -> Result<(), String> {
        lock(&self.sinks).insert(cell, sink);
        Ok(())
    }

    fn interrupt(&self, cell: &CellId) {
        if let Some(sink) = lock(&self.sinks).get(cell) {
            sink.finish(Some(CellFailure::interrupted("stopped")));
        }
    }

    fn shutdown(&self, _deadline: Duration) -> ShutdownReport {
        ShutdownReport::joined()
    }
}

struct NoTools;

impl NestedToolDispatcher for NoTools {
    fn dispatch(&self, call: NestedToolCall) -> NestedToolOutcome {
        NestedToolOutcome::error(&call, "unknown_tool", "none")
    }

    fn catalog(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::function(
            "read_file",
            "Reads a file.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": { "path": { "type": "string" } }
            }),
        )]
    }
}

/// Opens a session on the scripted engine, like the binary does on V8.
struct ScriptedFactory {
    engine: Arc<ScriptedEngine>,
}

impl CodeModeSessionFactory for ScriptedFactory {
    fn open(&self, id: SessionId) -> Result<Arc<CodeModeSession>, String> {
        Ok(Arc::new(CodeModeSession::new(
            id,
            Arc::clone(&self.engine) as Arc<_>,
        )))
    }
}

async fn handle() -> (Arc<CodeModeHandle>, Arc<ScriptedEngine>) {
    let engine = Arc::new(ScriptedEngine::default());
    let handle = Arc::new(CodeModeHandle::new(
        Arc::new(ScriptedFactory {
            engine: Arc::clone(&engine),
        }),
        NestedToolBinding::default(),
    ));
    handle.bind_thread("thread-a").await.expect("session opens");
    handle.bind_step(Arc::new(NoTools), true);
    (handle, engine)
}

fn registry(handle: Arc<CodeModeHandle>) -> Registry {
    Registry::builder(std::env::temp_dir())
        .register(ExecTool::new(Arc::clone(&handle)))
        .register(WaitTool::new(handle))
        .build()
}

/// US-002 seen from the Registry: `exec` reaches the model as a FREEFORM tool
/// carrying its grammar, with no invented schema.
#[tokio::test]
async fn the_registry_exposes_exec_as_a_freeform_tool() {
    let (handle, _engine) = handle().await;
    let specs = registry(handle).tool_specs();
    let exec = specs
        .iter()
        .find(|spec| spec.name == EXEC_TOOL_NAME)
        .expect("exec is exposed");
    assert!(exec.is_freeform());
    assert_eq!(exec.input_schema(), None);
    exec.validate().expect("the exposed spec must be valid");

    let wait = specs
        .iter()
        .find(|spec| spec.name == WAIT_TOOL_NAME)
        .expect("wait is exposed");
    assert!(!wait.is_freeform());
    wait.validate().expect("the exposed spec must be valid");
}

/// The catalog bound for the step is what the model is told it can call.
#[tokio::test]
async fn the_exec_description_carries_the_nested_catalog() {
    let (handle, _engine) = handle().await;
    let description = ExecTool::new(handle).description();
    assert!(
        description
            .contains("declare function read_file(input: { path: string }): Promise<string>;"),
        "{description}"
    );
}

/// US-008 AC1 end to end: a freeform call dispatched by the Registry starts a
/// cell and comes back with its output and its cell identifier.
#[tokio::test]
async fn a_freeform_exec_call_runs_a_cell_through_the_registry() {
    let (handle, engine) = handle().await;
    let registry = registry(Arc::clone(&handle));

    let call = ToolInvocation {
        id: "call-1".into(),
        name: EXEC_TOOL_NAME.into(),
        input: serde_json::Value::String("text('hello');".into()),
        format: agent_core::message::ToolCallFormat::Text,
    };
    let dispatch = tokio::spawn({
        let registry = registry;
        async move { registry.dispatch(vec![call]).await }
    });

    // The scripted engine only moves when the test tells it to.
    let cell = loop {
        let cells = handle.session().expect("a session is bound").cells();
        if let Some((cell, _)) = cells.first() {
            break cell.clone();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    engine.sink(&cell).push_text("hello");
    engine.sink(&cell).finish(None);

    let outcomes = dispatch.await.expect("dispatch must not panic");
    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert!(!outcome.is_error, "{}", outcome.content);
    assert!(outcome.content.contains("hello"), "{}", outcome.content);
    assert!(
        outcome.content.contains("completed"),
        "the terminal state is always named: {}",
        outcome.content
    );
}

/// US-008 AC4: a malformed pragma is refused by the pipeline before a cell
/// exists, so the model gets a fixable error and nothing ran.
#[tokio::test]
async fn a_malformed_pragma_is_refused_before_a_cell_is_created() {
    let (handle, _engine) = handle().await;
    let registry = registry(Arc::clone(&handle));
    let call = ToolInvocation {
        id: "call-1".into(),
        name: EXEC_TOOL_NAME.into(),
        input: serde_json::Value::String("// @exec: {nope}\ntext(1);".into()),
        format: agent_core::message::ToolCallFormat::Text,
    };
    let outcomes = registry.dispatch(vec![call]).await;
    assert!(outcomes[0].is_error);
    assert!(
        handle
            .session()
            .expect("a session is bound")
            .cells()
            .is_empty(),
        "no cell was created"
    );
}

#[tokio::test]
async fn waiting_on_a_cell_of_another_thread_is_refused() {
    let (handle, _engine) = handle().await;
    let registry = registry(handle);
    let call = ToolInvocation::json(
        "call-1",
        WAIT_TOOL_NAME,
        serde_json::json!({ "cell_id": "thread-b#0" }),
    );
    let outcomes = registry.dispatch(vec![call]).await;
    assert!(outcomes[0].is_error);
    assert!(
        outcomes[0].content.contains("does not belong"),
        "{}",
        outcomes[0].content
    );
}

#[test]
fn a_data_url_splits_into_a_media_type_and_a_payload() {
    assert_eq!(
        split_data_url("data:image/png;base64,QUJD"),
        Some(("image/png".to_string(), "QUJD".to_string()))
    );
    assert_eq!(split_data_url("https://example.invalid/a.png"), None);
    assert_eq!(split_data_url("data:image/png,QUJD"), None);
}

#[test]
fn a_failed_cell_is_reported_as_an_error_with_its_category() {
    let output = render(RuntimeResponse::Finished {
        cell_id: CellId::new(&SessionId::new("thread-a"), 0),
        items: vec![OutputItem::text("partial")],
        failure: Some(CellFailure::new(CellFailureKind::CpuBudget, "30000 ms")),
        omitted_bytes: 12,
    });
    assert!(output.is_error);
    assert!(output.content.contains("partial"));
    assert!(output.content.contains("cpu_budget"));
    assert!(output.content.contains("12 octets omis"));
}

/// US-009 AC3: a new thread gets a NEW session. The previous one is closed, and
/// nothing a cell stored survives into the next conversation.
#[tokio::test]
async fn binding_another_thread_opens_a_fresh_session_and_closes_the_previous_one() {
    let (handle, engine) = handle().await;
    let first = handle.session().expect("a session is bound");
    let started = first
        .execute(ExecuteRequest::new("call-1", "noop").with_yield_time(Duration::from_millis(20)))
        .await
        .expect("the cell starts");
    assert!(first.cell_state(started.cell_id()).is_some());
    let _ = engine;

    handle.bind_thread("thread-b").await.expect("session opens");
    let second = handle.session().expect("a session is bound");
    assert_ne!(second.id().as_str(), first.id().as_str());
    assert!(second.cells().is_empty(), "the new session starts empty");
    assert!(first.is_closed(), "the previous session was shut down");
}

/// Edge case 1 of the PRD: without a session, `exec` refuses by NAMING the
/// missing runtime instead of failing with an empty answer.
#[tokio::test]
async fn exec_without_a_session_refuses_and_names_the_missing_runtime() {
    let engine = Arc::new(ScriptedEngine::default());
    let handle = Arc::new(CodeModeHandle::new(
        Arc::new(ScriptedFactory { engine }),
        NestedToolBinding::default(),
    ));
    handle.bind_step(Arc::new(NoTools), true);
    let registry = registry(handle);
    let call = ToolInvocation {
        id: "call-1".into(),
        name: EXEC_TOOL_NAME.into(),
        input: serde_json::Value::String("text(1);".into()),
        format: agent_core::message::ToolCallFormat::Text,
    };
    let outcomes = registry.dispatch(vec![call]).await;
    assert!(outcomes[0].is_error);
    assert!(
        outcomes[0].content.contains("Code Mode session"),
        "{}",
        outcomes[0].content
    );
}
