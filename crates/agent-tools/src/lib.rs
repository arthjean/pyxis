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

pub mod bash;
pub mod command;
pub mod edit;
pub mod error;
pub mod glob;
pub mod grep;
pub mod hooks;
pub mod path;
pub mod permission;
pub mod read;
pub mod registry;
pub mod shell;
pub mod taint;
pub mod tool;
pub mod turn_diff;
pub mod write;

#[cfg(test)]
mod tests_integration;

pub use bash::Bash;
pub use command::{CommandClass, classify};
pub use edit::Edit;
pub use error::{ToolError, ValidationError};
pub use glob::Glob;
pub use grep::Grep;
pub use hooks::{CommandHooks, HookDecision, HookEvent, HookSpec, Hooks, NoHooks};
pub use permission::{
    ApprovalEntry, ApprovalKey, ApprovalMemo, ApprovalMemory, ApprovalResponse, Approver,
    AutoApprove, AutoDeny, PermCtx, PermissionDecision, PermissionMode, PermissionModeState,
    PermissionRequest, Resolved, resolve_permission,
};
pub use read::Read;
pub use registry::{Registry, RegistryBuilder};
pub use shell::ShellChoice;
pub use tool::{CommandHardener, DynTool, DynToolAdapter, Tool, ToolCtx, ToolOutput, into_dyn};
pub use write::Write;

use std::sync::Arc;

/// Builds a `Registry` wired with the 6 base tools (Read, Glob, Grep,
/// Write, Edit, Bash): what agent-cli will inject as `Arc<dyn ToolDispatch>`.
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
        .build()
}
