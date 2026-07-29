//! US-019 AC3: what a Code Mode cell leaves on the trace, and how it stays
//! correlated to the call that opened it.
//!
//! A binary of its own, for the reason `agent-tools/tests/tracing_facade`
//! documents: `tracing` caches callsite interest GLOBALLY while
//! `subscriber::set_default` installs on the CURRENT THREAD only. A sibling test
//! running with no subscriber caches "never" for the `cell` span callsite, and
//! the correlation assertion then observes an event with no span context — which
//! is exactly how this test failed before it moved here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_code_mode::protocol::{CellState, ExecuteRequest, SessionId};
use agent_code_mode::session::{CodeModeSession, SessionLimits};
use agent_code_mode_v8::{EngineLimits, IsolateEngine};

struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut sink) => sink.extend_from_slice(buf),
            Err(poisoned) => poisoned.into_inner().extend_from_slice(buf),
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// AC3, the whole chain across the OS-thread boundary a cell runs on.
///
/// The parent span stands in for the `tool` span the registry opens inside the
/// `turn` span the runtime opens, so what is proven is
/// thread -> turn -> call -> cell. Neither the span nor the collector crosses
/// that boundary on its own, which is why the engine carries both.
#[tokio::test]
async fn a_failed_cell_traces_its_kind_correlated_to_its_caller() {
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || SharedBuffer(Arc::clone(&sink)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let engine = Arc::new(IsolateEngine::new(EngineLimits::default()).expect("v8 initializes"));
    let session = CodeModeSession::with_limits(
        SessionId::new("traced"),
        engine,
        SessionLimits {
            terminate_grace: Duration::from_millis(500),
            ..SessionLimits::default()
        },
    );
    let caller = tracing::info_span!("tool", thread_id = "th_9", turn_id = "tn_9", tool = "exec");
    let response = {
        use tracing::Instrument as _;
        session
            .execute(
                ExecuteRequest::new("call-1", "throw new Error('traced boom');")
                    .with_yield_time(Duration::from_secs(5)),
            )
            .instrument(caller)
            .await
            .unwrap()
    };
    assert_eq!(response.state(), CellState::Failed);

    let bytes = buffer.lock().expect("buffer").clone();
    let trace = String::from_utf8_lossy(&bytes).into_owned();
    let line = trace
        .lines()
        .find(|line| line.contains("code mode cell failed"))
        .unwrap_or_default();
    assert!(line.contains("thread_id=\"th_9\""), "{trace}");
    assert!(line.contains("turn_id=\"tn_9\""), "{trace}");
    assert!(line.contains("cell_id="), "{trace}");
    assert!(line.contains("kind=\"script_error\""), "{trace}");
    // The script and its message are content: they belong to the cell result,
    // not to a file the user is told to share.
    assert!(!trace.contains("traced boom"), "{trace}");
}
