//! Per-request model-visible context (US-006).
//!
//! `AgentContext` fixes what a turn is: model, system prompt, transcript,
//! limits. But the catalog of tools, the project context and the environment
//! fragments can move WHILE a turn runs (an MCP server connects, `/init` writes
//! an AGENTS.md, a skill is invoked). Rebuilding them once per turn would hide
//! the change until an artificial end of turn; rebuilding them mid-stream would
//! make the active sampling incoherent.
//!
//! So the loop asks a source for ONE frame before each model request, and that
//! frame is immutable for the whole request. The core neither reads a file nor
//! knows what a skill is: it only consumes [`StepFrame`].

use crate::message::Message;
use crate::provider::ToolSpec;

/// Model-visible context of exactly one model request.
///
/// Two frames sharing a `generation` are byte-identical, which is what lets the
/// loop skip re-estimating the static prompt and what keeps the cacheable prefix
/// stable across steps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepFrame {
    pub generation: u64,
    pub tools: Vec<ToolSpec>,
    pub context_messages: Vec<Message>,
}

/// Rebuilds the model-visible context before EVERY model request.
///
/// Implementations live outside the core (`agent_runtime::context`). A call sits
/// on the hot path: it must be cheap and must not block, so a source that needs
/// I/O caches its sections and refreshes them elsewhere.
pub trait StepContextSource: Send + Sync {
    fn next_frame(&self) -> StepFrame;
}
