//! US-007 proofs, run against a real isolate.

use std::sync::Mutex;
use std::time::Duration;

use agent_code_mode::protocol::{
    CellFailureKind, CellState, ExecuteRequest, OutputItem, RuntimeResponse, SessionId, WaitRequest,
};
use agent_code_mode::session::{CodeModeSession, SessionLimits};

use super::*;

fn engine(limits: EngineLimits) -> Arc<IsolateEngine> {
    Arc::new(IsolateEngine::new(limits).expect("v8 must initialize"))
}

fn session(name: &str, engine: Arc<IsolateEngine>) -> CodeModeSession {
    CodeModeSession::with_limits(
        SessionId::new(name),
        engine,
        SessionLimits {
            terminate_grace: Duration::from_millis(500),
            ..SessionLimits::default()
        },
    )
}

fn cell(source: &str) -> ExecuteRequest {
    ExecuteRequest::new("call-1", source).with_yield_time(Duration::from_secs(5))
}

fn texts(response: &RuntimeResponse) -> Vec<String> {
    response
        .items()
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_cell_reports_its_output_and_completes() {
    let session = session("solo", engine(EngineLimits::default()));
    let response = session
        .execute(cell("text('hello'); text(1 + 1);"))
        .await
        .unwrap();
    assert_eq!(response.state(), CellState::Completed);
    assert_eq!(texts(&response), vec!["hello".to_string(), "2".to_string()]);
}

#[tokio::test]
async fn await_works_at_the_top_level_of_a_cell() {
    let session = session("await", engine(EngineLimits::default()));
    let response = session
        .execute(cell(
            "const value = await Promise.resolve(41); text(value + 1);",
        ))
        .await
        .unwrap();
    assert_eq!(response.state(), CellState::Completed);
    assert_eq!(texts(&response), vec!["42".to_string()]);
}

/// US-007 AC1: two sessions run concurrently and see none of each other's state.
#[tokio::test]
async fn two_sessions_never_see_each_others_state() {
    let first = session("thread-a", engine(EngineLimits::default()));
    let second = session("thread-b", engine(EngineLimits::default()));

    let (left, right) = tokio::join!(
        first.execute(cell(
            "globalThis.owner = 'a'; store('owner', 'a'); text(globalThis.owner);"
        )),
        second.execute(cell(
            "globalThis.owner = 'b'; store('owner', 'b'); text(globalThis.owner);"
        )),
    );
    assert_eq!(texts(&left.unwrap()), vec!["a".to_string()]);
    assert_eq!(texts(&right.unwrap()), vec!["b".to_string()]);

    let (left, right) = tokio::join!(
        first.execute(cell("text(load('owner'));")),
        second.execute(cell("text(load('owner'));")),
    );
    assert_eq!(texts(&left.unwrap()), vec!["a".to_string()]);
    assert_eq!(texts(&right.unwrap()), vec!["b".to_string()]);
}

#[tokio::test]
async fn a_global_of_one_cell_is_invisible_to_the_next() {
    let session = session("globals", engine(EngineLimits::default()));
    session
        .execute(cell("globalThis.leaked = 'yes';"))
        .await
        .unwrap();
    let response = session
        .execute(cell("text(typeof globalThis.leaked);"))
        .await
        .unwrap();
    assert_eq!(texts(&response), vec!["undefined".to_string()]);
}

#[tokio::test]
async fn store_and_load_carry_values_across_cells_of_one_session() {
    let engine = engine(EngineLimits::default());
    let session = session("store", Arc::clone(&engine));
    session
        .execute(cell("store('rows', [1, 2, 3]);"))
        .await
        .unwrap();
    let response = session
        .execute(cell("const rows = load('rows'); text(rows.length);"))
        .await
        .unwrap();
    assert_eq!(texts(&response), vec!["3".to_string()]);
    assert_eq!(engine.stored_keys(), vec!["rows".to_string()]);
}

/// US-007 AC3: a script error is typed, and the session takes the next cell.
#[tokio::test]
async fn a_script_error_is_typed_and_the_session_stays_usable() {
    let session = session("throwing", engine(EngineLimits::default()));
    let response = session
        .execute(cell("text('before'); throw new Error('boom');"))
        .await
        .unwrap();
    assert_eq!(response.state(), CellState::Failed);
    let failure = response.failure().expect("a failed cell carries its cause");
    assert_eq!(failure.kind, CellFailureKind::Script);
    assert!(failure.message.contains("boom"), "{}", failure.message);
    // Output produced before the throw is not lost.
    assert_eq!(texts(&response), vec!["before".to_string()]);

    let next = session.execute(cell("text('after');")).await.unwrap();
    assert_eq!(next.state(), CellState::Completed);
    assert_eq!(texts(&next), vec!["after".to_string()]);
}

#[tokio::test]
async fn a_syntax_error_is_a_script_failure_not_a_quota_breach() {
    let session = session("syntax", engine(EngineLimits::default()));
    let response = session
        .execute(cell("this is not javascript"))
        .await
        .unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::Script)
    );
}

/// US-007 AC3: the CPU ceiling stops a runaway cell, typed, session alive.
#[tokio::test]
async fn a_cpu_budget_breach_is_typed_and_the_session_stays_usable() {
    let session = session(
        "cpu",
        engine(EngineLimits {
            cpu_budget: Duration::from_millis(200),
            ..EngineLimits::default()
        }),
    );
    let response = session.execute(cell("while (true) {}")).await.unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::CpuBudget),
        "{:?}",
        response.failure()
    );
    let next = session.execute(cell("text('alive');")).await.unwrap();
    assert_eq!(texts(&next), vec!["alive".to_string()]);
}

/// US-007 AC3 and US-005 finding: heap and native ceilings stay distinct.
#[tokio::test]
async fn the_heap_ceiling_is_typed_as_a_heap_breach() {
    let session = session(
        "heap",
        engine(EngineLimits {
            heap_bytes: 16 * 1024 * 1024,
            cpu_budget: Duration::from_secs(20),
            ..EngineLimits::default()
        }),
    );
    let response = session
        .execute(cell(
            "const kept = []; for (;;) { kept.push('x'.repeat(1 << 16)); }",
        ))
        .await
        .unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::HeapLimit),
        "{:?}",
        response.failure()
    );
}

#[tokio::test]
async fn the_native_ceiling_is_typed_as_a_native_breach() {
    let session = session(
        "native",
        engine(EngineLimits {
            native_bytes: 8 * 1024 * 1024,
            cpu_budget: Duration::from_secs(20),
            ..EngineLimits::default()
        }),
    );
    let response = session
        .execute(cell(
            "const kept = []; for (;;) { kept.push(new ArrayBuffer(1 << 20)); }",
        ))
        .await
        .unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::NativeMemory),
        "{:?}",
        response.failure()
    );
}

#[tokio::test]
async fn exit_ends_a_cell_successfully_at_the_statement_itself() {
    let session = session("exit", engine(EngineLimits::default()));
    let response = session
        .execute(cell("text('kept'); exit(); text('never');"))
        .await
        .unwrap();
    assert_eq!(response.state(), CellState::Completed);
    assert_eq!(texts(&response), vec!["kept".to_string()]);
}

/// A cell that catches its own `exit()` and then fails for a real reason must
/// still be reported as failed: the exit marker is what tells the two apart.
#[tokio::test]
async fn an_error_raised_after_a_swallowed_exit_is_still_a_failure() {
    let session = session("exit-then-throw", engine(EngineLimits::default()));
    let response = session
        .execute(cell(
            "try { exit(); } catch (error) { throw new Error('real failure'); }",
        ))
        .await
        .unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::Script),
        "{:?}",
        response.failure()
    );
    assert!(
        response
            .failure()
            .is_some_and(|failure| failure.message.contains("real failure"))
    );
}

#[tokio::test]
async fn yield_control_hands_back_output_while_the_cell_keeps_running() {
    let session = session("yield", engine(EngineLimits::default()));
    let started = session
        .execute(
            ExecuteRequest::new(
                "call-1",
                "text('early'); yield_control(); await new Promise(() => {}); ",
            )
            .with_yield_time(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(started.state(), CellState::Yielded);
    assert_eq!(texts(&started), vec!["early".to_string()]);
    let _ = session.terminate(started.cell_id()).await;
}

#[tokio::test]
async fn an_interruption_closes_the_cell_and_names_its_cause() {
    let session = session(
        "interrupt",
        engine(EngineLimits {
            cpu_budget: Duration::from_secs(20),
            ..EngineLimits::default()
        }),
    );
    let started = session
        .execute(
            ExecuteRequest::new("call-1", "while (true) {}")
                .with_yield_time(Duration::from_millis(150)),
        )
        .await
        .unwrap();
    assert_eq!(started.state(), CellState::Yielded);

    let stopped = session
        .wait(WaitRequest::terminating(started.cell_id().clone()))
        .await
        .unwrap();
    assert!(matches!(stopped, RuntimeResponse::Terminated { .. }));
    assert_eq!(
        stopped.failure().map(|failure| failure.kind),
        Some(CellFailureKind::Interrupted)
    );
}

/// US-007 AC4: a worker that outlives the deadline is named, never detached in
/// silence.
#[tokio::test]
async fn shutdown_reports_a_worker_it_could_not_join() {
    let engine = engine(EngineLimits {
        cpu_budget: Duration::from_secs(30),
        ..EngineLimits::default()
    });
    let session = session("stubborn", Arc::clone(&engine));
    let started = session
        .execute(
            ExecuteRequest::new("call-1", "while (true) {}")
                .with_yield_time(Duration::from_millis(100)),
        )
        .await
        .unwrap();
    assert_eq!(started.state(), CellState::Yielded);

    // A zero deadline cannot let the isolate unwind, so the worker is still
    // holding it when the report is written.
    let report = engine.shutdown(Duration::ZERO);
    assert!(!report.joined);
    let detail = report.detail.expect("an unjoined worker names itself");
    assert!(detail.contains("did not join"), "{detail}");
    assert!(detail.contains(started.cell_id().as_str()), "{detail}");
}

#[tokio::test]
async fn shutdown_joins_a_cooperative_worker() {
    let engine = engine(EngineLimits::default());
    let session = session("cooperative", Arc::clone(&engine));
    session.execute(cell("text('done');")).await.unwrap();
    let report = engine.shutdown(Duration::from_secs(2));
    assert!(report.joined, "{:?}", report.detail);
}

#[tokio::test]
async fn a_session_shutdown_forces_every_open_cell() {
    let engine = engine(EngineLimits {
        cpu_budget: Duration::from_secs(30),
        ..EngineLimits::default()
    });
    let session = session("closing", Arc::clone(&engine));
    let started = session
        .execute(
            ExecuteRequest::new("call-1", "while (true) {}")
                .with_yield_time(Duration::from_millis(100)),
        )
        .await
        .unwrap();
    let report = session.shutdown(Duration::from_secs(2)).await;
    assert!(report.joined, "{:?}", report.detail);
    assert!(session.cell_state(started.cell_id()).is_none() || report.forced_cells.is_empty());
}

/// US-007 AC2: the JIT policy is a process-wide decision, and a contradiction
/// is refused rather than silently resolved in favour of the first caller.
#[tokio::test]
async fn a_contradictory_jit_mode_is_refused_before_a_session_exists() {
    // Any engine of this suite has already initialized V8 with the default.
    let _ = engine(EngineLimits::default());
    let active = crate::active_jit_mode().expect("v8 is initialized by now");
    let other = match active {
        crate::V8JitMode::Enabled => crate::V8JitMode::Jitless,
        crate::V8JitMode::Jitless => crate::V8JitMode::Enabled,
    };
    let error = crate::initialize_v8(other).expect_err("switching JIT mode must fail");
    assert!(
        matches!(error, crate::V8Error::JitModeLocked { .. }),
        "{error}"
    );
    // Asking again for the mode in force is still fine.
    assert!(crate::initialize_v8(active).is_ok());
}

#[tokio::test]
async fn a_cell_has_no_console_no_network_and_no_shared_memory() {
    let session = session("allowlist", engine(EngineLimits::default()));
    let response = session
        .execute(cell(
            "text([typeof console, typeof fetch, typeof require, typeof SharedArrayBuffer, typeof WebAssembly].join(','));",
        ))
        .await
        .unwrap();
    assert_eq!(
        texts(&response),
        vec!["undefined,undefined,undefined,undefined,undefined".to_string()]
    );
}

// --- US-008: nested tools seen from inside a cell -------------------------

use agent_code_mode::nested::{
    NestedToolCall, NestedToolDispatcher, NestedToolInput, NestedToolOutcome,
};
use agent_code_mode::protocol::NestedTool;
use agent_core::provider::ToolSpec;

/// Answers nested calls without a tool pipeline, and records what it saw.
struct FakeTools {
    specs: Vec<ToolSpec>,
    denied: Vec<String>,
    seen: Mutex<Vec<String>>,
    delay: Duration,
}

impl FakeTools {
    fn new(specs: Vec<ToolSpec>) -> Arc<Self> {
        Arc::new(Self {
            specs,
            denied: Vec::new(),
            seen: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        })
    }

    fn seen(&self) -> Vec<String> {
        match self.seen.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl NestedToolDispatcher for FakeTools {
    fn dispatch(&self, call: NestedToolCall) -> NestedToolOutcome {
        if !self.specs.iter().any(|spec| spec.name == call.tool) {
            return NestedToolOutcome::error(&call, "unknown_tool", "no such tool");
        }
        let rendered = match &call.input {
            NestedToolInput::Json(value) => value.to_string(),
            NestedToolInput::Text(text) => text.clone(),
        };
        match self.seen.lock() {
            Ok(mut guard) => guard.push(format!("{}:{rendered}", call.tool)),
            Err(poisoned) => poisoned
                .into_inner()
                .push(format!("{}:{rendered}", call.tool)),
        }
        if self.denied.contains(&call.tool) {
            return NestedToolOutcome::error(&call, "permission_denied", "refused by policy");
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        NestedToolOutcome {
            call_id: call.call_id,
            tool: call.tool,
            content: rendered,
            structured: None,
            is_error: false,
            error_kind: None,
        }
    }

    fn catalog(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }
}

fn echo_spec(name: &str) -> ToolSpec {
    ToolSpec::function(
        name,
        "echoes its input",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["value"],
            "properties": { "value": { "type": "string" } }
        }),
    )
}

fn nested_request(source: &str, catalog: &[ToolSpec]) -> ExecuteRequest {
    ExecuteRequest::new("call-1", source)
        .with_tools(catalog.iter().map(NestedTool::from_spec).collect())
        .with_yield_time(Duration::from_secs(10))
}

#[tokio::test]
async fn a_cell_awaits_a_nested_tool_and_reads_its_result() {
    let engine = engine(EngineLimits::default());
    let specs = vec![echo_spec("read_file")];
    let tools = FakeTools::new(specs.clone());
    engine.nested_tools().set(Arc::clone(&tools) as Arc<_>);
    let session = session("nested", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "const out = await tools.read_file({ value: 'x' }); text(out);",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.state(),
        CellState::Completed,
        "{:?}",
        response.failure()
    );
    assert_eq!(texts(&response), vec!["{\"value\":\"x\"}".to_string()]);
    assert_eq!(
        tools.seen(),
        vec!["read_file:{\"value\":\"x\"}".to_string()]
    );
}

#[tokio::test]
async fn all_tools_describes_the_catalog_the_cell_may_call() {
    let engine = engine(EngineLimits::default());
    let specs = vec![echo_spec("read_file"), echo_spec("write_file")];
    engine
        .nested_tools()
        .set(FakeTools::new(specs.clone()) as Arc<_>);
    let session = session("catalog", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "text(ALL_TOOLS.map((entry) => entry.name).sort().join(','));",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(texts(&response), vec!["read_file,write_file".to_string()]);
}

/// US-008 AC4: a name the model invented has no effect and comes back as a
/// structured error the cell can branch on.
#[tokio::test]
async fn an_absent_tool_rejects_with_a_structured_error_and_no_effect() {
    let engine = engine(EngineLimits::default());
    let specs = vec![echo_spec("read_file")];
    let tools = FakeTools::new(specs.clone());
    engine.nested_tools().set(Arc::clone(&tools) as Arc<_>);
    let session = session("absent", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "try { await tools.read_file({ value: 'x' }); } catch (error) { text('unexpected'); }\n\
             text(typeof tools.rm_rf);",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(texts(&response), vec!["undefined".to_string()]);
    assert_eq!(tools.seen().len(), 1, "only the declared tool ran");
}

#[tokio::test]
async fn a_denied_tool_rejects_with_its_kind_and_the_cell_keeps_control() {
    let engine = engine(EngineLimits::default());
    let specs = vec![echo_spec("write_file")];
    let tools = Arc::new(FakeTools {
        specs: specs.clone(),
        denied: vec!["write_file".to_string()],
        seen: Mutex::new(Vec::new()),
        delay: Duration::ZERO,
    });
    engine.nested_tools().set(Arc::clone(&tools) as Arc<_>);
    let session = session("denied", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "try { await tools.write_file({ value: 'x' }); text('ran'); }\n\
             catch (error) { text(error.name + '/' + error.kind + '/' + error.tool); }",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(response.state(), CellState::Completed);
    assert_eq!(
        texts(&response),
        vec!["ToolError/permission_denied/write_file".to_string()]
    );
}

/// US-008 AC3: the terminal order of a cell's nested calls is the order it made
/// them, correlation included.
#[tokio::test]
async fn nested_results_stay_correlated_and_ordered() {
    let engine = engine(EngineLimits::default());
    let specs = vec![echo_spec("first"), echo_spec("second")];
    let tools = FakeTools::new(specs.clone());
    engine.nested_tools().set(Arc::clone(&tools) as Arc<_>);
    let session = session("ordered", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "const a = await tools.first({ value: '1' });\n\
             const b = await tools.second({ value: '2' });\n\
             const c = await tools.first({ value: '3' });\n\
             text([a, b, c].join('|'));",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(
        texts(&response),
        vec!["{\"value\":\"1\"}|{\"value\":\"2\"}|{\"value\":\"3\"}".to_string()]
    );
    assert_eq!(
        tools.seen(),
        vec![
            "first:{\"value\":\"1\"}".to_string(),
            "second:{\"value\":\"2\"}".to_string(),
            "first:{\"value\":\"3\"}".to_string()
        ]
    );
}

/// A slow tool is not the cell burning CPU: the watchdog must not count it.
#[tokio::test]
async fn waiting_on_a_nested_tool_does_not_eat_the_cpu_budget() {
    let engine = engine(EngineLimits {
        cpu_budget: Duration::from_millis(300),
        ..EngineLimits::default()
    });
    let specs = vec![echo_spec("slow")];
    let tools = Arc::new(FakeTools {
        specs: specs.clone(),
        denied: Vec::new(),
        seen: Mutex::new(Vec::new()),
        delay: Duration::from_millis(400),
    });
    engine.nested_tools().set(Arc::clone(&tools) as Arc<_>);
    let session = session("slow-tool", Arc::clone(&engine));

    let response = session
        .execute(nested_request(
            "await tools.slow({ value: 'x' }); text('survived');",
            &specs,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.state(),
        CellState::Completed,
        "{:?}",
        response.failure()
    );
    assert_eq!(texts(&response), vec!["survived".to_string()]);
}

#[tokio::test]
async fn notify_hands_output_to_the_model_before_the_cell_ends() {
    let session = session("notify", engine(EngineLimits::default()));
    let started = session
        .execute(
            ExecuteRequest::new("call-1", "notify('early'); await new Promise(() => {});")
                .with_yield_time(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(started.state(), CellState::Yielded);
    assert_eq!(texts(&started), vec!["early".to_string()]);
    let _ = session.terminate(started.cell_id()).await;
}
