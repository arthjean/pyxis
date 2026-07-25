//! Permission model (5 modes, ARCHITECTURE 4.4) + taint defense (4.6,
//! OWASP LLM01). The final decision combines: the current mode, the tool's own
//! *baseline* decision, its nature (read-only / sensitive), and the presence
//! of **recent taint**. A mutating or sensitive action triggered in the presence
//! of recent taint **forces `Ask`** in every mode except
//! `BypassPermissions` (invariant 3).
//!
//! The interactive boundary is the `Approver` trait: the pipeline does not know
//! *how* the question is asked (TUI, `-p`, auto), it calls `approve()`. Testable
//! headlessly through a scripted approver.

use agent_core::message::ToolCallId;
use async_trait::async_trait;
use std::sync::{Arc, RwLock};

/// The 5 permission modes (ARCHITECTURE 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Asks on a sensitive action.
    #[default]
    Default,
    /// Auto-accepts file edits, asks for the rest.
    AcceptEdits,
    /// Never interrupts (controlled automations).
    DontAsk,
    /// Short-circuits every check (advanced use / under a sandbox).
    BypassPermissions,
    /// Read-only: no mutation allowed (planning phase).
    Plan,
}

/// Shared state of the current permission mode.
///
/// The registry keeps this handle and the TUI can update it in session through
/// `/permissions` without rebuilding the tools.
#[derive(Debug, Clone)]
pub struct PermissionModeState {
    inner: Arc<RwLock<PermissionMode>>,
}

impl PermissionModeState {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            inner: Arc::new(RwLock::new(mode)),
        }
    }

    pub fn get(&self) -> PermissionMode {
        match self.inner.read() {
            Ok(mode) => *mode,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub fn set(&self, mode: PermissionMode) {
        match self.inner.write() {
            Ok(mut current) => *current = mode,
            Err(poisoned) => *poisoned.into_inner() = mode,
        }
    }
}

impl Default for PermissionModeState {
    fn default() -> Self {
        Self::new(PermissionMode::default())
    }
}

/// A tool's *baseline* decision for a given input, before the global rules
/// (mode + taint) are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allowed without confirmation.
    Allow,
    /// Human confirmation required.
    Ask,
    /// Forbidden (the tool will not run).
    Deny,
}

/// Context passed to `Tool::permission` to decide the baseline.
#[derive(Debug, Clone, Copy)]
pub struct PermCtx {
    pub mode: PermissionMode,
    /// Was untrusted taint produced recently? (injection defense.)
    pub taint_recent: bool,
}

/// Outcome of the final resolution (what the Registry applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Run directly.
    Allow,
    /// Ask the `Approver` for confirmation before running.
    Ask,
    /// Refuse (do not run, return an error to the agent).
    Deny,
}

/// Resolves the final decision. PURE (no I/O) -> unit-testable.
///
/// Priority:
/// 1. Tool `Deny` -> always `Deny` (terminal invariant).
/// 2. `BypassPermissions` -> `Allow` for actions that only require a user
///    confirmation.
/// 3. `Plan` -> `Allow` when the tool is read-only, `Deny` otherwise (no mutation).
/// 4. Otherwise: start from the tool baseline, shape it according to the mode,
///    then **taint** forces `Ask` for a mutating/sensitive action (except Bypass, already
///    handled): invariant 3 / section 4.6.
pub fn resolve_permission(
    mode: PermissionMode,
    baseline: PermissionDecision,
    is_read_only: bool,
    is_sensitive: bool,
    is_taint_sensitive: bool,
    taint_recent: bool,
) -> Resolved {
    if baseline == PermissionDecision::Deny {
        return Resolved::Deny;
    }
    // Bypass: short-circuits user confirmations, not hard denies.
    if mode == PermissionMode::BypassPermissions {
        return Resolved::Allow;
    }
    // 2. Plan: strict read-only.
    if mode == PermissionMode::Plan {
        return if is_read_only {
            Resolved::Allow
        } else {
            Resolved::Deny
        };
    }

    // 3. Shaping the baseline according to the mode.
    let shaped = match baseline {
        PermissionDecision::Deny => Resolved::Deny,
        PermissionDecision::Allow => Resolved::Allow,
        PermissionDecision::Ask => match mode {
            // Default: we honor the request.
            PermissionMode::Default => Resolved::Ask,
            // AcceptEdits: auto-accepts (non-sensitive) edits; keeps the
            // request on sensitive actions (destructive/network).
            PermissionMode::AcceptEdits => {
                if is_sensitive {
                    Resolved::Ask
                } else {
                    Resolved::Allow
                }
            }
            // DontAsk: never interrupts (subject to taint, below).
            PermissionMode::DontAsk => Resolved::Allow,
            // Plan / Bypass already handled.
            PermissionMode::Plan | PermissionMode::BypassPermissions => Resolved::Allow,
        },
    };

    // Taint: a mutating/sensitive action in a tainted context forces confirmation, whatever
    // the mode (except Bypass, already returned). This is the direct mitigation
    // of indirect injection (section 4.6).
    if taint_recent && is_taint_sensitive && shaped == Resolved::Allow {
        return Resolved::Ask;
    }
    shaped
}

/// Confirmation request presented to the user (through the `Approver`).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    /// True when the request is forced by recent untrusted content.
    pub taint_forced: bool,
    pub mode: String,
    /// Short summary of the input (e.g. the Bash command, the written path).
    pub input_summary: String,
    /// Raw structured input: lets the frontend render a rich preview
    /// (diff for `edit`, command for `bash`) in the permission dialog.
    pub input: serde_json::Value,
}

/// Interactive boundary: the pipeline delegates confirmation here. The CLI/TUI
/// provides a real implementation (prompt); tests a scripted double.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Returns `true` when the action is allowed.
    async fn approve(&self, req: &PermissionRequest) -> bool;
}

/// Automatic approver. By default it accepts routine requests but
/// refuses requests forced by taint.
#[derive(Debug, Clone, Copy)]
pub struct AutoApprove {
    approve_tainted: bool,
}

impl AutoApprove {
    pub const fn new() -> Self {
        Self {
            approve_tainted: false,
        }
    }

    pub const fn including_tainted() -> Self {
        Self {
            approve_tainted: true,
        }
    }
}

impl Default for AutoApprove {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, req: &PermissionRequest) -> bool {
        !req.taint_forced || self.approve_tainted
    }
}

/// Approver that refuses everything (fail-closed: safe default in headless mode
/// without a counterpart, or for refusal-path tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _req: &PermissionRequest) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bash-like: sensitive, mutating. Edit-like: non-sensitive, mutating but protected
    // by taint. Read-like: read-only, non-sensitive.
    const SENSITIVE: (bool, bool, bool) = (
        /*read_only*/ false, /*sensitive*/ true, /*taint*/ true,
    );
    const EDIT: (bool, bool, bool) = (false, false, true);
    const READ: (bool, bool, bool) = (true, false, false);

    fn res(
        mode: PermissionMode,
        base: PermissionDecision,
        kind: (bool, bool, bool),
        taint: bool,
    ) -> Resolved {
        resolve_permission(mode, base, kind.0, kind.1, kind.2, taint)
    }

    #[test]
    fn bypass_short_circuits_everything() {
        // Even a tainted sensitive action passes under Bypass.
        assert_eq!(
            res(
                PermissionMode::BypassPermissions,
                PermissionDecision::Ask,
                SENSITIVE,
                true
            ),
            Resolved::Allow
        );
    }

    #[test]
    fn bypass_does_not_override_explicit_deny() {
        assert_eq!(
            res(
                PermissionMode::BypassPermissions,
                PermissionDecision::Deny,
                SENSITIVE,
                true
            ),
            Resolved::Deny
        );
    }

    #[test]
    fn plan_is_read_only() {
        assert_eq!(
            res(PermissionMode::Plan, PermissionDecision::Allow, READ, false),
            Resolved::Allow
        );
        // Every mutation is refused under Plan, even a baseline Allow.
        assert_eq!(
            res(PermissionMode::Plan, PermissionDecision::Allow, EDIT, false),
            Resolved::Deny
        );
        assert_eq!(
            res(
                PermissionMode::Plan,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Deny
        );
    }

    #[test]
    fn default_mode_asks_on_sensitive_allows_reads() {
        // US-013 AC1: Default -> asks on a mutating/network action.
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Ask
        );
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Allow,
                READ,
                false
            ),
            Resolved::Allow
        );
    }

    #[test]
    fn accept_edits_auto_accepts_edits_keeps_ask_on_sensitive() {
        // Edit (non-sensitive, baseline Ask) -> auto-accepted.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                EDIT,
                false
            ),
            Resolved::Allow
        );
        // Sensitive action -> stays Ask.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Ask
        );
    }

    #[test]
    fn taint_forces_ask_overriding_dontask() {
        // US-013 AC3 / section 4.6: DontAsk would allow, but recent taint + sensitive
        // -> forced confirmation (mode override, except Bypass).
        assert_eq!(
            res(
                PermissionMode::DontAsk,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Allow,
            "without taint, DontAsk does not interrupt"
        );
        assert_eq!(
            res(
                PermissionMode::DontAsk,
                PermissionDecision::Ask,
                SENSITIVE,
                true
            ),
            Resolved::Ask,
            "with recent taint, the sensitive action forces confirmation"
        );
    }

    #[test]
    fn taint_forces_ask_on_edits_without_breaking_accept_edits() {
        // An edit stays auto-accepted without taint.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                EDIT,
                false
            ),
            Resolved::Allow
        );
        // But taint also protects mutations that are not sensitive in the normal sense.
        assert_eq!(
            res(PermissionMode::DontAsk, PermissionDecision::Ask, EDIT, true),
            Resolved::Ask
        );
    }

    #[test]
    fn taint_does_not_force_ask_on_read_only_tools() {
        assert_eq!(
            res(PermissionMode::DontAsk, PermissionDecision::Ask, READ, true),
            Resolved::Allow
        );
    }

    #[test]
    fn explicit_deny_is_terminal() {
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Deny,
                READ,
                false
            ),
            Resolved::Deny
        );
    }
}
