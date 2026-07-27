//! Tool dispatch contract (injected). The real implementation (registry,
//! permissions, taint, pipeline) is `agent-tools` (EP-003); the core only knows
//! this trait. In EP-002, a mock is enough to close the stream -> tool loop.

use tokio::sync::mpsc;

use crate::event::{PermissionReq, PlanView};
use crate::message::{ToolCallId, ToolErrorKind};

/// A tool call requested by the model (args already reassembled into valid JSON).
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of a tool. `is_error` marks an application failure; `untrusted`
/// carries the taint (OWASP LLM01) decided by the `agent-tools` pipeline from
/// `Tool::returns_untrusted()`, fail-closed to `true` by default (US-013).
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub id: ToolCallId,
    pub content: String,
    pub is_error: bool,
    pub untrusted: bool,
    pub error_kind: Option<ToolErrorKind>,
    /// Images the tool brought into the turn (US-011). A `tool_result` block
    /// carries text only, so the loop turns these into a user message right
    /// after the result. Empty for every other tool.
    pub images: Vec<ToolImage>,
}

impl ToolOutcome {
    /// Text outcome, the shape every tool but `view_image` produces.
    pub fn new(
        id: ToolCallId,
        content: String,
        is_error: bool,
        untrusted: bool,
        error_kind: Option<ToolErrorKind>,
    ) -> Self {
        Self {
            id,
            content,
            is_error,
            untrusted,
            error_kind,
            images: Vec::new(),
        }
    }
}

/// Image a tool read for the model (US-011). Base64 payload plus its media
/// type: the exact pair `ContentBlock::Image` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolImage {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone)]
pub enum ToolDispatchEvent {
    PermissionAsk(PermissionReq),
    /// Output fragment of a tool still running (US-015), correlated by `id`.
    OutputDelta {
        id: ToolCallId,
        chunk: String,
    },
    /// Structured plan a tool just published (US-009). Travels beside the
    /// textual result because it is addressed to the client, not to the model.
    Plan(PlanView),
}

#[derive(Clone, Default)]
pub struct ToolEventSink {
    tx: Option<mpsc::UnboundedSender<ToolDispatchEvent>>,
}

impl ToolEventSink {
    pub fn new(tx: mpsc::UnboundedSender<ToolDispatchEvent>) -> Self {
        Self { tx: Some(tx) }
    }

    pub fn emit(&self, event: ToolDispatchEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
}

#[async_trait::async_trait]
pub trait ToolDispatch: Send + Sync {
    /// Reseeds the permission taint from a resumed transcript. Implementations
    /// without taint can ignore this signal.
    fn seed_taint(&self, _recent_untrusted: bool) {}

    /// Runs a batch of calls and returns their results (order not guaranteed;
    /// each result is correlated by `id`).
    async fn dispatch(&self, calls: Vec<ToolInvocation>, events: ToolEventSink)
    -> Vec<ToolOutcome>;
}
