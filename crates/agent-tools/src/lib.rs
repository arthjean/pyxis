//! `agent-tools`: tool system & execution guardrails (EP-003). Implements
//! the core `ToolDispatch` trait (`agent-core`): a `Registry` that dispatches
//! a tool batch (concurrent/serial) through a **strict pipeline**:
//! parse -> validate -> permission -> call (timeout) -> taint, with a 5-mode
//! permission model and the untrusted taint defense (OWASP LLM01).
//!
//! Invariants held: fail-closed `Tool` trait (4), untrusted output by default
//! (3), one `ToolOutcome` per call (never a panic, correlation by `id`).
//! The loop/budget guardrails (US-014) live in `agent-core` (the graph
//! forbids `core -> tools`; stopping the loop is a core decision).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod ask;
pub mod bash;
pub mod code_mode;
pub mod command;
pub mod context_window;
pub mod edit;
pub mod error;
pub mod exec_session;
pub mod glob;
pub mod grep;
pub mod hooks;
pub mod image;
pub mod patch;
pub mod path;
pub mod permission;
pub mod plan;
#[cfg(unix)]
pub mod pty;
pub mod read;
pub mod registry;
pub mod sandbox;
pub mod shell;
pub mod taint;
pub mod time;
pub mod tool_search;
pub mod tool;
pub mod turn_diff;
pub mod write;

#[cfg(test)]
mod tests_integration;

pub use agent::{
    AgentHandle, FollowupTask, InterruptAgent, ListAgents, MULTI_AGENT_V2_TOOLS, SendMessage,
    SpawnAgent, WaitAgent,
};
pub use ask::{
    GrantOutcome, PermissionAsk, PermissionBroker, RequestPermissions, RequestUserInput, UserNotice,
};
pub use bash::Bash;
pub use code_mode::{CodeModeHandle, CodeModeSessionFactory, ExecTool, WaitTool};
pub use command::{CommandClass, classify};
pub use context_window::{GetContextRemaining, NewContextWindow};
pub use edit::Edit;
pub use error::{ToolError, ValidationError};
pub use exec_session::{ExecCommand, ExecSessions, WriteStdin};
pub use glob::Glob;
pub use grep::Grep;
pub use hooks::{
    CommandHooks, HOOK_EVENT_NAMES, HookDecision, HookEvent, HookSpec, Hooks, Lifecycle, NoHooks,
};
pub use image::ViewImage;
pub use patch::ApplyPatch;
pub use permission::{
    ApprovalEntry, ApprovalKey, ApprovalMemo, ApprovalMemory, ApprovalResponse, Approver,
    AutoApprove, AutoDeny, PermCtx, PermissionDecision, PermissionMode, PermissionModeState,
    PermissionRequest, Resolved, resolve_permission,
};
pub use plan::UpdatePlan;
pub use read::Read;
pub use registry::{Registry, RegistryBuilder};
pub use sandbox::{
    EscalationGuard, SandboxDenial, SandboxEscalator, SandboxObserver, classify_failure,
};
pub use shell::ShellChoice;
pub use time::{CurrentTime, Sleep};
pub use tool_search::{DEFER_THRESHOLD, DeferredEntry, DeferredTools, ToolSearch};
pub use tool::{CommandHardener, DynTool, DynToolAdapter, Tool, ToolCtx, ToolOutput, into_dyn};
pub use write::Write;

use std::sync::Arc;

/// Builds a `Registry` wired with the native tools: what agent-cli injects as
/// `Arc<dyn ToolDispatch>`. Six of them predate EP-003 (Read, Glob, Grep,
/// Write, Edit, Bash); the four families ported by that epic bring the count to
/// eleven, `exec_command` and `write_stdin` being one family split in two tools
/// like Codex does.
pub fn default_registry(
    workspace: impl Into<std::path::PathBuf>,
    mode: PermissionMode,
    approver: Arc<dyn Approver>,
) -> Registry {
    Registry::builder(workspace)
        .mode(mode)
        .approver(approver)
        .register(Read)
        .register(Glob)
        .register(Grep)
        .register(Write)
        .register(Edit)
        .register(Bash)
        .register(UpdatePlan)
        .register(ApplyPatch)
        .register(ViewImage)
        .register(ExecCommand)
        .register(WriteStdin)
        .build()
}
