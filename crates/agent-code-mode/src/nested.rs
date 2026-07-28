//! Nested tool calls: `tools.<name>(args)` from inside a cell.
//!
//! The whole point of this module is that a nested call is NOT a second tool
//! pipeline. It reuses the `ToolDispatchSnapshot` captured for the step that
//! called `exec`, so the taint, the hooks, the permission mode, the approval
//! memory and the cancellation a direct call would meet are the same ones a
//! nested call meets. Code Mode changes what the MODEL sees, never what the
//! policy decides.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::message::{ToolCallFormat, ToolErrorKind};
use agent_core::provider::ToolSpec;
use agent_core::tools::{
    StepToolPlan, ToolDispatchSnapshot, ToolEventSink, ToolInvocation, ToolResultStatus,
};

use crate::protocol::CellId;

/// Input of a nested call, in the algebra US-002 generalized: a function tool
/// takes JSON, a freeform tool takes text. A cell cannot turn one into the
/// other.
#[derive(Debug, Clone, PartialEq)]
pub enum NestedToolInput {
    Json(serde_json::Value),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NestedToolCall {
    pub cell_id: CellId,
    /// Identifier the cell correlates its promise with.
    pub call_id: String,
    pub tool: String,
    pub input: NestedToolInput,
}

/// Result of a nested call as the cell reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedToolOutcome {
    pub call_id: String,
    pub tool: String,
    pub content: String,
    pub structured: Option<serde_json::Value>,
    pub is_error: bool,
    /// Machine-readable category, present on every error so the cell can branch
    /// on it instead of matching a message.
    pub error_kind: Option<&'static str>,
}

impl NestedToolOutcome {
    pub fn error(call: &NestedToolCall, kind: &'static str, content: impl Into<String>) -> Self {
        Self {
            call_id: call.call_id.clone(),
            tool: call.tool.clone(),
            content: content.into(),
            structured: None,
            is_error: true,
            error_kind: Some(kind),
        }
    }
}

/// Runs one nested call. Implementations are called from the CELL thread,
/// which is a plain OS thread, so blocking there is legitimate.
pub trait NestedToolDispatcher: Send + Sync {
    fn dispatch(&self, call: NestedToolCall) -> NestedToolOutcome;

    /// Tools a cell may call, for the catalog the cell sees.
    fn catalog(&self) -> Vec<ToolSpec>;
}

/// Dispatcher over the step's own tool plan.
pub struct PlanDispatcher {
    plan: StepToolPlan,
    events: ToolEventSink,
    runtime: tokio::runtime::Handle,
    callable: HashSet<String>,
    ordinal: AtomicU64,
}

impl PlanDispatcher {
    /// Builds the nested view of `snapshot`.
    ///
    /// `hidden` are the model-facing Code Mode tools (`exec`, `wait`): a cell
    /// calling them would either recurse into itself or drive the session it
    /// runs in, so they are removed from the nested catalog rather than left
    /// callable and failing late.
    pub fn new(
        snapshot: &ToolDispatchSnapshot,
        hidden: &[&str],
        events: ToolEventSink,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let specs: Vec<ToolSpec> = snapshot
            .specs()
            .iter()
            .filter(|spec| !hidden.contains(&spec.name.as_str()))
            .cloned()
            .collect();
        let callable = specs.iter().map(|spec| spec.name.clone()).collect();
        // Serial dispatch: one nested call at a time, so the terminal order of
        // a cell's calls is the order the cell made them.
        let plan = StepToolPlan::capture(snapshot.with_specs(specs), false);
        Self {
            plan,
            events,
            runtime,
            callable,
            ordinal: AtomicU64::new(0),
        }
    }

    /// Identifier given to the underlying tool call. It carries the cell and an
    /// ordinal, so a result can be traced back to the exact nested call even
    /// when a cell calls the same tool repeatedly.
    fn invocation_id(&self, call: &NestedToolCall) -> String {
        format!(
            "{}::{}::{}",
            call.cell_id.as_str(),
            call.call_id,
            self.ordinal.fetch_add(1, Ordering::AcqRel)
        )
    }
}

impl NestedToolDispatcher for PlanDispatcher {
    fn dispatch(&self, call: NestedToolCall) -> NestedToolOutcome {
        if !self.callable.contains(&call.tool) {
            // Refused BEFORE the pipeline: an unknown name never becomes an
            // effect, and the cell learns why.
            return NestedToolOutcome::error(
                &call,
                "unknown_tool",
                format!("tool `{}` is not available in this cell", call.tool),
            );
        }

        let (input, format) = match &call.input {
            NestedToolInput::Json(value) => (value.clone(), ToolCallFormat::Json),
            NestedToolInput::Text(text) => (
                serde_json::Value::String(text.clone()),
                ToolCallFormat::Text,
            ),
        };
        let invocation = ToolInvocation {
            id: self.invocation_id(&call),
            name: call.tool.clone(),
            input,
            format,
        };

        let events = self.events.clone();
        let plan = self.plan.clone();
        // The cell thread is not a Tokio worker, so blocking it here parks
        // exactly one OS thread and never the runtime.
        let mut outcomes = self
            .runtime
            .block_on(async move { plan.dispatch(vec![invocation], events).await });
        let Some(outcome) = outcomes.pop() else {
            return NestedToolOutcome::error(
                &call,
                "engine_error",
                "the tool pipeline returned no result",
            );
        };

        NestedToolOutcome {
            call_id: call.call_id,
            tool: call.tool,
            content: outcome.content,
            structured: outcome.structured_content,
            is_error: outcome.is_error,
            error_kind: outcome
                .error_kind
                .map(error_kind_label)
                .or_else(|| status_label(outcome.status)),
        }
    }

    fn catalog(&self) -> Vec<ToolSpec> {
        self.plan.specs().to_vec()
    }
}

fn error_kind_label(kind: ToolErrorKind) -> &'static str {
    match kind {
        ToolErrorKind::UnknownTool => "unknown_tool",
        ToolErrorKind::Parse => "parse",
        ToolErrorKind::Validation => "validation",
        ToolErrorKind::OutsideWorkspace => "outside_workspace",
        ToolErrorKind::Io => "io",
        ToolErrorKind::Rejected => "rejected",
        ToolErrorKind::PermissionDenied => "permission_denied",
        ToolErrorKind::SandboxDenied => "sandbox_denied",
        ToolErrorKind::Timeout => "timeout",
        ToolErrorKind::Cancelled => "cancelled",
        ToolErrorKind::Semantic => "semantic",
    }
}

/// A refusal that carries no `error_kind` still has a status; the cell must
/// never see an error without a category.
fn status_label(status: ToolResultStatus) -> Option<&'static str> {
    match status {
        ToolResultStatus::Success => None,
        ToolResultStatus::Error => Some("error"),
        ToolResultStatus::Rejected => Some("rejected"),
        ToolResultStatus::Cancelled => Some("cancelled"),
        ToolResultStatus::TimedOut => Some("timeout"),
        ToolResultStatus::SandboxDenied => Some("sandbox_denied"),
    }
}

/// Dispatcher that refuses everything, for a session started without one.
pub struct NoNestedTools;

impl NestedToolDispatcher for NoNestedTools {
    fn dispatch(&self, call: NestedToolCall) -> NestedToolOutcome {
        NestedToolOutcome::error(
            &call,
            "unknown_tool",
            "no nested tool is available in this cell",
        )
    }

    fn catalog(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
}

/// Shared handle so the exec tool can bind the dispatcher of the step in
/// flight without threading it through the serializable `ExecuteRequest`.
#[derive(Clone, Default)]
pub struct NestedToolBinding {
    inner: Arc<std::sync::Mutex<Option<Arc<dyn NestedToolDispatcher>>>>,
}

impl NestedToolBinding {
    pub fn set(&self, dispatcher: Arc<dyn NestedToolDispatcher>) {
        *self.lock() = Some(dispatcher);
    }

    pub fn clear(&self) {
        *self.lock() = None;
    }

    pub fn get(&self) -> Option<Arc<dyn NestedToolDispatcher>> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<dyn NestedToolDispatcher>>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[path = "nested_tests.rs"]
mod tests;
