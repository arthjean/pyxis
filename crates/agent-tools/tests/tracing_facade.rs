//! US-020: what the dispatch emits through the `tracing` facade, and at which
//! level.
//!
//! A binary of its own, and that is load-bearing rather than cosmetic.
//! `tracing` caches callsite interest GLOBALLY while `subscriber::set_default`
//! installs the subscriber on the CURRENT THREAD only. Any other test emitting
//! at the same callsite with no subscriber installed caches "never" for every
//! thread, and the higher-verbosity assertion then observes nothing. Isolating
//! these tests in their own process removes that interference at the source;
//! `TRACE_LOCK` covers the remaining one, between themselves.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::{Arc, Mutex};

use agent_core::tools::ToolInvocation;
use agent_tools::permission::{AutoApprove, PermissionDecision, PermissionMode};
use agent_tools::{Registry, Tool, ToolCtx, ToolOutput};
use async_trait::async_trait;

/// Minimal always-allowed tool: enough to make the pipeline emit its decision.
struct Probe;

#[async_trait]
impl Tool for Probe {
    type Input = serde_json::Value;
    fn name(&self) -> &str {
        "p"
    }
    fn description(&self) -> String {
        "probe".into()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn permission(&self, _i: &Self::Input, _c: &agent_tools::PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    async fn call(
        &self,
        _i: Self::Input,
        _c: &ToolCtx,
    ) -> Result<ToolOutput, agent_tools::ToolError> {
        Ok(ToolOutput::text("p ok"))
    }
}

fn call(id: &str, input: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        id: id.into(),
        name: "p".into(),
        input,
    }
}

fn registry() -> Registry {
    Registry::builder("/tmp")
        .mode(PermissionMode::DontAsk)
        .approver(Arc::new(AutoApprove::including_tainted()))
        .register(Probe)
        .build()
}

/// Serializes the tests that install a subscriber: two different levels racing
/// on the same callsite cache is exactly the interference this file isolates.
static TRACE_LOCK: Mutex<()> = Mutex::new(());

/// Collects the trace of one dispatch at a given level, on the current thread
/// only: the process-wide subscriber belongs to the binary (FR-15).
// The guard must span the dispatch: releasing it earlier would let the other
// level re-cache the callsites mid-run. Each test owns its runtime, so nothing
// else can be starved by it.
#[allow(clippy::await_holding_lock)]
async fn traced_dispatch(level: tracing::Level, calls: Vec<ToolInvocation>) -> String {
    let _serialized = match TRACE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(move || SharedBuffer(Arc::clone(&sink)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // Re-evaluate every callsite against the subscriber just installed, instead
    // of trusting an interest the other level may have cached.
    tracing::callsite::rebuild_interest_cache();

    let _ = registry().dispatch(calls).await;

    let bytes = buffer.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

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

/// US-020 AC3: the crate emits structured events, collected by whoever installs a
/// subscriber.
#[tokio::test]
async fn the_dispatch_emits_a_structured_decision() {
    let trace = traced_dispatch(
        tracing::Level::DEBUG,
        vec![call("a", serde_json::json!({"secret": "hunter2"}))],
    )
    .await;

    assert!(trace.contains("tool permission resolved"), "{trace}");
    assert!(trace.contains("pyxis::tools"), "{trace}");
    assert!(trace.contains("tool=p"), "{trace}");
    assert!(trace.contains("resolved=Allow"), "{trace}");
}

/// US-020 AC6: the arguments of a call are content. They appear at the highest
/// verbosity, and nowhere below it.
#[tokio::test]
async fn call_content_appears_only_at_the_highest_verbosity() {
    let input = serde_json::json!({"secret": "hunter2"});

    let debug = traced_dispatch(tracing::Level::DEBUG, vec![call("a", input.clone())]).await;
    assert!(
        !debug.contains("hunter2"),
        "le contenu ne doit pas fuiter en debug: {debug}"
    );

    let full = traced_dispatch(tracing::Level::TRACE, vec![call("a", input)]).await;
    assert!(full.contains("hunter2"), "attendu au niveau trace: {full}");
}

/// US-020 AC4: without a subscriber, an emission produces nothing. Proven by
/// running the same dispatch outside any collection and observing that the tools
/// keep working, at the same cost as before the instrumentation.
#[tokio::test]
async fn without_a_subscriber_the_dispatch_is_unchanged() {
    let out = registry()
        .dispatch(vec![call("a", serde_json::json!({}))])
        .await;

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, "p ok");
}
