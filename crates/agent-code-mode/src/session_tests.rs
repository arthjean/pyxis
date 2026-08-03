//! US-006 proofs. The engine is a scripted stub: every cell transition is
//! driven by the test, so what is under test is the session, never V8.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;
use crate::protocol::{CellFailure, CellFailureKind};

/// Engine whose cells only move when the test tells them to.
#[derive(Default)]
struct ScriptedEngine {
    sinks: Mutex<HashMap<CellId, CellSink>>,
    interrupted: Mutex<Vec<CellId>>,
    start_error: Mutex<Option<String>>,
    /// `true` makes `interrupt` finish the cell, like an engine that obeys.
    obeys_interrupt: AtomicBool,
    shutdown_joined: AtomicBool,
}

impl ScriptedEngine {
    fn obedient() -> Arc<Self> {
        let engine = Arc::new(Self::default());
        engine.obeys_interrupt.store(true, Ordering::Release);
        engine.shutdown_joined.store(true, Ordering::Release);
        engine
    }

    fn deaf() -> Arc<Self> {
        let engine = Arc::new(Self::default());
        engine.shutdown_joined.store(true, Ordering::Release);
        engine
    }

    fn sink(&self, cell: &CellId) -> CellSink {
        let sinks = match self.sinks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sinks.get(cell).cloned().expect("cell was started")
    }

    fn interrupted(&self) -> Vec<CellId> {
        match self.interrupted.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl CellEngine for ScriptedEngine {
    fn start(&self, cell: CellId, _request: &ExecuteRequest, sink: CellSink) -> Result<(), String> {
        let error = match self.start_error.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(error) = error {
            return Err(error);
        }
        match self.sinks.lock() {
            Ok(mut guard) => guard.insert(cell, sink),
            Err(poisoned) => poisoned.into_inner().insert(cell, sink),
        };
        Ok(())
    }

    fn interrupt(&self, cell: &CellId) {
        match self.interrupted.lock() {
            Ok(mut guard) => guard.push(cell.clone()),
            Err(poisoned) => poisoned.into_inner().push(cell.clone()),
        }
        if self.obeys_interrupt.load(Ordering::Acquire) {
            let sink = {
                let sinks = match self.sinks.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                sinks.get(cell).cloned()
            };
            if let Some(sink) = sink {
                sink.finish(Some(CellFailure::interrupted("engine stopped the cell")));
            }
        }
    }

    fn shutdown(&self, _deadline: Duration) -> ShutdownReport {
        if self.shutdown_joined.load(Ordering::Acquire) {
            ShutdownReport::joined()
        } else {
            ShutdownReport::unjoined("worker thread did not join")
        }
    }
}

fn session(engine: Arc<ScriptedEngine>) -> CodeModeSession {
    CodeModeSession::with_limits(
        SessionId::new("thread-a"),
        engine,
        SessionLimits {
            terminate_grace: Duration::from_millis(80),
            max_active_cells: 2,
            max_output_bytes: 64,
        },
    )
}

fn quick(source: &str) -> ExecuteRequest {
    ExecuteRequest::new("call-1", source).with_yield_time(Duration::from_millis(40))
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
async fn a_first_cell_gets_a_stable_id_and_walks_its_states() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));

    let started = session.execute(quick("noop")).await.unwrap();
    let cell = started.cell_id().clone();
    assert_eq!(started.state(), CellState::Yielded);
    assert!(cell.belongs_to(session.id()));
    assert_eq!(session.cell_state(&cell), Some(CellState::Yielded));

    engine.sink(&cell).push_text("hello");
    engine.sink(&cell).finish(None);

    let finished = session
        .wait(WaitRequest::new(cell.clone()).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(finished.cell_id(), &cell, "the identifier is stable");
    assert_eq!(finished.state(), CellState::Completed);
    assert_eq!(texts(&finished), vec!["hello".to_string()]);
    // A terminal response closes the cell: nothing can be delivered twice.
    assert_eq!(session.cell_state(&cell), None);
}

#[tokio::test]
async fn wait_returns_only_the_output_produced_since_the_last_yield() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));

    let started = session.execute(quick("noop")).await.unwrap();
    let cell = started.cell_id().clone();
    assert!(texts(&started).is_empty());

    engine.sink(&cell).push_text("first");
    engine.sink(&cell).request_yield();
    let first = session
        .wait(WaitRequest::new(cell.clone()).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(texts(&first), vec!["first".to_string()]);

    engine.sink(&cell).push_text("second");
    engine.sink(&cell).finish(None);
    let second = session
        .wait(WaitRequest::new(cell.clone()).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(
        texts(&second),
        vec!["second".to_string()],
        "the first chunk must not be delivered again"
    );
}

#[tokio::test]
async fn a_yielded_cell_reports_its_state_between_two_waits() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();
    assert_eq!(session.cells(), vec![(cell.clone(), CellState::Yielded)]);
    engine.sink(&cell).push_text("progress");
    assert_eq!(session.cell_state(&cell), Some(CellState::Running));
}

#[tokio::test]
async fn terminate_closes_a_cell_an_obedient_engine_stops() {
    let engine = ScriptedEngine::obedient();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let stopped = session
        .wait(WaitRequest::terminating(cell.clone()))
        .await
        .unwrap();
    assert!(matches!(stopped, RuntimeResponse::Terminated { .. }));
    assert_eq!(stopped.state(), CellState::Failed);
    assert_eq!(
        stopped.failure().map(|failure| failure.kind),
        Some(CellFailureKind::Interrupted)
    );
    assert_eq!(engine.interrupted(), vec![cell.clone()]);
    assert_eq!(session.cell_state(&cell), None);
}

#[tokio::test]
async fn terminate_forces_a_terminal_state_when_the_engine_stays_deaf() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let stopped = session.terminate(&cell).await.unwrap();
    assert_eq!(stopped.state(), CellState::Failed);
    let failure = stopped.failure().expect("a forced stop carries its cause");
    assert_eq!(failure.kind, CellFailureKind::Interrupted);
    assert!(
        failure.message.contains("did not confirm"),
        "the forced case must say so: {}",
        failure.message
    );
    assert_eq!(session.cell_state(&cell), None);
}

#[tokio::test]
async fn a_waiter_is_woken_by_the_terminal_state() {
    let engine = ScriptedEngine::deaf();
    let session = Arc::new(session(Arc::clone(&engine)));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let waiter = {
        let session = Arc::clone(&session);
        let cell = cell.clone();
        // A generous yield time: only the wake-up can end this wait early.
        tokio::spawn(async move {
            session
                .wait(WaitRequest::new(cell).with_yield_time(Duration::from_secs(30)))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    engine
        .sink(&cell)
        .finish(Some(CellFailure::new(CellFailureKind::Script, "boom")));

    let response = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("the waiter must be woken, not left parked")
        .unwrap()
        .unwrap();
    assert_eq!(response.state(), CellState::Failed);
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::Script)
    );
}

#[tokio::test]
async fn shutdown_forces_every_open_cell_and_wakes_its_waiters() {
    let engine = ScriptedEngine::deaf();
    let session = Arc::new(session(Arc::clone(&engine)));
    let first = session
        .execute(quick("one"))
        .await
        .unwrap()
        .cell_id()
        .clone();
    let second = session
        .execute(quick("two"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let waiter = {
        let session = Arc::clone(&session);
        let cell = first.clone();
        tokio::spawn(async move {
            session
                .wait(WaitRequest::new(cell).with_yield_time(Duration::from_secs(30)))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;

    let report = session.shutdown(Duration::from_millis(100)).await;
    assert!(report.joined);
    assert_eq!(report.forced_cells, vec![first.clone(), second.clone()]);
    assert_eq!(engine.interrupted().len(), 2);

    let response = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("shutdown must wake the waiter")
        .unwrap()
        .unwrap();
    assert_eq!(response.state(), CellState::Failed);
    assert!(session.is_closed());
    assert!(matches!(
        session.execute(quick("late")).await,
        Err(CodeModeError::SessionClosed { .. })
    ));
}

#[tokio::test]
async fn an_unjoined_worker_is_reported_rather_than_hidden() {
    let engine = Arc::new(ScriptedEngine::default());
    let session = session(engine);
    let report = session.shutdown(Duration::from_millis(50)).await;
    assert!(!report.joined);
    assert_eq!(report.detail.as_deref(), Some("worker thread did not join"));
}

#[tokio::test]
async fn unknown_foreign_and_stale_identifiers_fail_in_a_typed_way() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let unknown = CellId::new(session.id(), 99);
    assert!(matches!(
        session.wait(WaitRequest::new(unknown)).await,
        Err(CodeModeError::UnknownCell { .. })
    ));

    let foreign = CellId::new(&SessionId::new("thread-b"), 0);
    assert!(matches!(
        session.wait(WaitRequest::new(foreign.clone())).await,
        Err(CodeModeError::ForeignCell { .. })
    ));
    assert!(matches!(
        session.terminate(&foreign).await,
        Err(CodeModeError::ForeignCell { .. })
    ));

    let shapeless = CellId::parse("not-a-cell").unwrap();
    assert!(matches!(
        session.wait(WaitRequest::new(shapeless)).await,
        Err(CodeModeError::ForeignCell { .. })
    ));

    // The live cell was never touched by any of the refusals above.
    engine.sink(&cell).push_text("still mine");
    let mine = session
        .wait(WaitRequest::new(cell).with_yield_time(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_eq!(texts(&mine), vec!["still mine".to_string()]);
}

#[tokio::test]
async fn a_foreign_identifier_never_reveals_another_sessions_output() {
    let engine = ScriptedEngine::deaf();
    let other = CodeModeSession::new(SessionId::new("thread-b"), Arc::clone(&engine) as Arc<_>);
    let secret = other
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();
    engine.sink(&secret).push_text("secret");

    let session = session(Arc::clone(&engine));
    let error = session
        .wait(WaitRequest::new(secret.clone()))
        .await
        .unwrap_err();
    assert!(matches!(error, CodeModeError::ForeignCell { .. }));
    // The output is still there for its rightful owner.
    let mine = other
        .wait(WaitRequest::new(secret).with_yield_time(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_eq!(texts(&mine), vec!["secret".to_string()]);
}

#[tokio::test]
async fn a_failing_engine_start_leaves_no_cell_behind() {
    let engine = ScriptedEngine::deaf();
    match engine.start_error.lock() {
        Ok(mut guard) => *guard = Some("isolate unavailable".into()),
        Err(poisoned) => *poisoned.into_inner() = Some("isolate unavailable".into()),
    }
    let session = session(Arc::clone(&engine));
    let error = session.execute(quick("noop")).await.unwrap_err();
    assert!(matches!(error, CodeModeError::EngineUnavailable { .. }));
    assert!(session.cells().is_empty());
}

#[tokio::test]
async fn the_active_cell_bound_is_refused_without_consuming_a_slot() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    session.execute(quick("one")).await.unwrap();
    session.execute(quick("two")).await.unwrap();
    let error = session.execute(quick("three")).await.unwrap_err();
    assert!(matches!(
        error,
        CodeModeError::TooManyCells { limit: 2, .. }
    ));
    assert_eq!(session.cells().len(), 2);
}

#[tokio::test]
async fn output_beyond_the_result_ceiling_is_counted_not_silently_dropped() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    engine.sink(&cell).push_text("x".repeat(60));
    engine.sink(&cell).push_text("y".repeat(30));
    engine.sink(&cell).finish(None);

    let response = session
        .wait(WaitRequest::new(cell).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(texts(&response), vec!["x".repeat(60)]);
    let RuntimeResponse::Finished { omitted_bytes, .. } = response else {
        unreachable!("expected a finished cell");
    };
    assert_eq!(omitted_bytes, 30, "omitted bytes are counted exactly");
}

#[tokio::test]
async fn a_late_terminal_cannot_reopen_a_cell_nor_rewrite_its_cause() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    let sink = engine.sink(&cell);
    sink.finish(Some(CellFailure::new(CellFailureKind::CpuBudget, "budget")));
    sink.finish(None);
    sink.push_text("after the end");

    let response = session
        .wait(WaitRequest::new(cell).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(
        response.failure().map(|failure| failure.kind),
        Some(CellFailureKind::CpuBudget),
        "the first terminal wins"
    );
    assert_eq!(texts(&response), vec!["after the end".to_string()]);
}

/// A cell whose ceiling is reached stops accepting output until the next
/// yield: a smaller item slipping in behind a dropped one would leave a hole
/// in the middle of the stream, and `omitted_bytes` says how much is missing,
/// never where.
#[tokio::test]
async fn the_result_ceiling_drops_the_rest_of_the_yield_not_just_the_big_item() {
    let engine = ScriptedEngine::deaf();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();

    engine.sink(&cell).push_text("x".repeat(60));
    engine.sink(&cell).push_text("y".repeat(30));
    engine.sink(&cell).push_text("z");
    engine.sink(&cell).request_yield();

    let response = session
        .wait(WaitRequest::new(cell.clone()).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(
        texts(&response),
        vec!["x".repeat(60)],
        "nothing may follow the first dropped item"
    );
    let RuntimeResponse::Yielded { omitted_bytes, .. } = response else {
        unreachable!("expected a yielded cell");
    };
    assert_eq!(omitted_bytes, 31);

    // The budget is restored by the yield: the next chunk goes through.
    engine.sink(&cell).push_text("fresh");
    engine.sink(&cell).finish(None);
    let next = session
        .wait(WaitRequest::new(cell).with_yield_time(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(texts(&next), vec!["fresh".to_string()]);
}

/// A closed session says so, rather than pretending the cell never existed.
#[tokio::test]
async fn a_closed_session_names_its_closure_on_a_cell_it_no_longer_holds() {
    let engine = ScriptedEngine::obedient();
    let session = session(Arc::clone(&engine));
    let cell = session
        .execute(quick("noop"))
        .await
        .unwrap()
        .cell_id()
        .clone();
    session.shutdown(Duration::from_millis(50)).await;

    // The forced cell is still drainable exactly once...
    let drained = session
        .wait(WaitRequest::new(cell.clone()).with_yield_time(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_eq!(drained.state(), CellState::Failed);
    // ...and everything after that is the closure itself, not an unknown cell.
    assert!(matches!(
        session.wait(WaitRequest::new(cell)).await,
        Err(CodeModeError::SessionClosed { .. })
    ));
    assert!(matches!(
        session
            .wait(WaitRequest::new(CellId::new(session.id(), 99)))
            .await,
        Err(CodeModeError::SessionClosed { .. })
    ));
}

#[test]
fn a_tool_name_becomes_a_valid_javascript_identifier() {
    assert_eq!(crate::normalize_binding("exec_command"), "exec_command");
    assert_eq!(crate::normalize_binding("apply-patch"), "apply_patch");
    assert_eq!(
        crate::normalize_binding("mcp__ologs__get_profile"),
        "mcp__ologs__get_profile"
    );
    assert_eq!(crate::normalize_binding("2fast"), "_2fast");
    // A substituted character is already a valid first character: only a
    // leading digit needs a prefix.
    assert_eq!(crate::normalize_binding("-lead"), "_lead");
    assert_eq!(crate::normalize_binding(""), "_");
}
