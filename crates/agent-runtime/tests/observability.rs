//! US-019 AC1/AC3: what a failure leaves behind, on the durable log and on the
//! trace.
//!
//! A binary of its own for the same reason as `agent-tools/tests/tracing_facade`:
//! `tracing` caches callsite interest globally while `subscriber::set_default`
//! installs on the current thread only, so a test emitting at the same callsite
//! without a subscriber would poison the assertion for every other one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use agent_core::provider::ProviderError;
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::failure::{FailureCategory, TurnFailure};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::ThreadStore;
use agent_runtime::thread::Submission;

use common::{
    EchoTools, FakeProvider, FakeSession, Scripted, agent_context, deps, start, wait_for_terminal,
};

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

/// Runs one turn that fails on the provider, under a `debug` subscriber, and
/// returns (the trace, the causes the durable log recorded).
async fn failing_turn() -> (String, Vec<String>) {
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || SharedBuffer(Arc::clone(&sink)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let provider = FakeProvider::failing(vec![Scripted::OpenErr(ProviderError::Http {
        status: 400,
        message: "refusé".into(),
        retry_after_ms: None,
    })]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(provider, FakeSession::new(), Arc::new(EchoTools)),
        agent_context,
    ));
    let harness = start(runner).await;
    harness
        .handle
        .submit(Submission::new("vas-y"))
        .await
        .unwrap();
    let terminal = wait_for_terminal(&harness.handle).await;
    assert_eq!(terminal.state, TurnState::Failed);
    harness.handle.shutdown().await;

    let causes = harness
        .store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| match event.payload {
            ThreadEventPayload::TurnStateChanged { to, cause, .. } if to.is_terminal() => cause,
            _ => None,
        })
        .collect();
    let bytes = buffer.lock().unwrap().clone();
    (String::from_utf8_lossy(&bytes).into_owned(), causes)
}

/// AC1: the cause the durable log carries is the one the classifier reads, so
/// the four surfaces cannot disagree: they all start from this string.
///
/// AC3: at `debug`, the terminal failure is traced with its identifiers and its
/// category, under a span carrying the thread and the turn.
#[tokio::test]
async fn a_failed_turn_records_a_classifiable_cause_and_traces_it_correlated() {
    let (trace, causes) = failing_turn().await;

    assert_eq!(causes.len(), 1, "exactly one terminal cause: {causes:?}");
    let failure = TurnFailure::from_cause(&causes[0]);
    assert_eq!(
        failure.category,
        FailureCategory::Provider,
        "cause: {}",
        causes[0]
    );

    assert!(
        trace.contains("turn reached a terminal failure"),
        "the terminal failure is traced: {trace}"
    );
    assert!(
        trace.contains("category=\"provider\"") || trace.contains("category=provider"),
        "the trace carries the category: {trace}"
    );
    // The `turn` span is what correlates every line the turn produced, including
    // the ones emitted by the provider, the tools and a Code Mode cell.
    assert!(trace.contains("thread_id="), "{trace}");
    assert!(trace.contains("turn_id="), "{trace}");
}

/// AC3 privacy half: the submitted text is content and never reaches a trace
/// below `trace`. A prompt in a file the user is told to share is the failure
/// mode this guards.
#[tokio::test]
async fn the_trace_of_a_failed_turn_carries_no_prompt() {
    let (trace, _) = failing_turn().await;
    assert!(!trace.contains("vas-y"), "the prompt leaked: {trace}");
}
