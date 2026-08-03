//! Wiring of the Code Mode runtime into the binary (US-009).
//!
//! This is the only place in Pyxis that knows both that Code Mode exists and
//! that it is implemented on V8. `agent-tools` sees a session factory,
//! `agent-runtime` sees nothing at all, and a build without this module still
//! resolves every direct model.

use std::sync::Arc;

use agent_code_mode::{CodeModeSession, NestedToolBinding, SessionId};
use agent_code_mode_v8::{EngineLimits, IsolateEngine};
use agent_tools::{CodeModeHandle, CodeModeSessionFactory};

/// Opens one isolate-backed session per thread.
///
/// A session gets its OWN engine, hence its own value store and its own cell
/// workers: two threads of the same process share nothing, which is the
/// isolation US-007 measured and US-009 relies on.
struct V8SessionFactory {
    limits: EngineLimits,
    nested: NestedToolBinding,
}

impl CodeModeSessionFactory for V8SessionFactory {
    fn open(&self, id: SessionId) -> Result<Arc<CodeModeSession>, String> {
        let engine = IsolateEngine::with_nested_binding(self.limits, self.nested.clone())
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(CodeModeSession::new(id, Arc::new(engine))))
    }
}

/// Builds the handle the `exec`/`wait` pair and the step source share.
///
/// Failing to initialize V8 is NOT fatal: it returns `None`, the two tools are
/// never registered, the catalog keeps `code_mode = false`, and a model that
/// needs Code Mode is refused before any request with the cause named. A broken
/// JavaScript engine must not cost the user every direct model as well.
pub fn build() -> Option<Arc<CodeModeHandle>> {
    let nested = NestedToolBinding::default();
    let factory = V8SessionFactory {
        limits: EngineLimits::default(),
        nested: nested.clone(),
    };
    // Opening a throwaway session is what proves V8 really initializes here,
    // rather than discovering it on the first cell of a real turn.
    if let Err(detail) = factory.open(SessionId::new("probe")) {
        tracing::warn!(%detail, "code mode runtime unavailable");
        return None;
    }
    Some(Arc::new(CodeModeHandle::new(Arc::new(factory), nested)))
}
