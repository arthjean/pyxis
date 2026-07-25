//! `agent-tools` — système d'outils & garde-fous d'exécution (EP-003). Implémente
//! le trait `ToolDispatch` du cœur (`agent-core`) : un `Registry` qui dispatche
//! un batch d'outils (concurrent/série) à travers un **pipeline strict** —
//! parse → validate → permission → call (timeout) → taint — avec un modèle de
//! permissions à 5 modes et la défense taint untrusted (OWASP LLM01).
//!
//! Invariants tenus : trait `Tool` fail-closed (4), sortie untrusted par défaut
//! (3), un `ToolOutcome` par appel (jamais de panic, corrélation par `id`).
//! Les garde-fous de boucle/budget (US-014) vivent dans `agent-core` (le graphe
//! interdit `core → tools` ; l'arrêt de boucle est une décision du cœur).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bash;
pub mod edit;
pub mod error;
pub mod glob;
pub mod grep;
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
pub use edit::Edit;
pub use error::{ToolError, ValidationError};
pub use glob::Glob;
pub use grep::Grep;
pub use permission::{
    Approver, AutoApprove, AutoDeny, PermCtx, PermissionDecision, PermissionMode,
    PermissionModeState, PermissionRequest, Resolved, resolve_permission,
};
pub use read::Read;
pub use registry::{Registry, RegistryBuilder};
pub use shell::ShellChoice;
pub use tool::{CommandHardener, DynTool, DynToolAdapter, Tool, ToolCtx, ToolOutput, into_dyn};
pub use write::Write;

use std::sync::Arc;

/// Construit un `Registry` câblé avec les 6 outils de base (Read, Glob, Grep,
/// Write, Edit, Bash) — ce que l'agent-cli injectera comme `Arc<dyn ToolDispatch>`.
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
