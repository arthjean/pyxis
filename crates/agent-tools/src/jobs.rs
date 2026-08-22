//! Link between a tool and the background job registry of its thread (EP-042).
//!
//! The registry lives in `agent-runtime`, one per thread, and this crate holds
//! no thread. So the handle here is the same late binding
//! [`crate::agent::AgentHandle`] uses for the sub-agent supervisor: the binary
//! creates one, hands it to the tools, and rebinds it to the registry of the
//! thread it opens.
//!
//! An UNBOUND handle is not an error. A `ToolCtx` built outside a session, a
//! unit test, a tool exercised on its own: none of them has a thread, and a
//! terminal must still open in all three. What an unbound handle costs is the
//! accounting, not the behavior.

use std::sync::{Arc, RwLock};

use agent_runtime::jobs::JobRegistry;

/// The registry of the current thread, rebound each time a thread is opened.
#[derive(Default)]
pub struct JobHandle {
    current: RwLock<Option<Arc<JobRegistry>>>,
}

impl JobHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Points the handle at the registry of the thread being opened. The
    /// previous registry belongs to a thread that is closing; its jobs were
    /// already settled by its own teardown.
    pub fn bind(&self, registry: Arc<JobRegistry>) {
        if let Ok(mut current) = self.current.write() {
            *current = Some(registry);
        }
    }

    /// The registry, or `None` when nothing is bound.
    pub fn registry(&self) -> Option<Arc<JobRegistry>> {
        self.current.read().ok()?.clone()
    }
}
