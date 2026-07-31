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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The 5 permission modes. Defined in `agent-core` because a permission request
/// travels to the clients and must carry the mode as a VALUE, not as a debug
/// string; re-exported here so the pipeline keeps one import path.
pub use agent_core::permission::PermissionMode;

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

/// Can the answer to a confirmation be remembered for the session (US-008), and
/// under which key? Produced by the tool itself, which alone knows what
/// identifies one of its calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalMemo {
    /// Rememberable under this EXACT token sequence.
    Key(Vec<String>),
    /// Rememberable answers exist for this tool, but not for this call. The
    /// reason is shown to the user.
    Refused(&'static str),
    /// This tool has no notion of a repeatable call: no option is offered.
    NotApplicable,
}

/// Session approval key. Holds the exact argv token sequence and the directory
/// the command would run in: the same command elsewhere is not the same act.
///
/// Comparison is derived (elementwise on `Vec<String>`), so a remembered answer
/// can never be reached by a command that merely shares a string prefix
/// (CVE-2026-22708).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalKey {
    pub tool: String,
    pub tokens: Vec<String>,
    pub cwd: PathBuf,
}

impl ApprovalKey {
    pub fn new(tool: impl Into<String>, tokens: &[String], cwd: impl AsRef<Path>) -> Self {
        Self {
            tool: tool.into(),
            tokens: tokens.to_vec(),
            cwd: cwd.as_ref().to_path_buf(),
        }
    }

    /// Human-readable form for the inspection surface (`/approvals`).
    pub fn display(&self) -> String {
        self.tokens.join(" ")
    }
}

/// One remembered answer, as exposed to the inspection surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEntry {
    pub tool: String,
    pub command: String,
    pub allow: bool,
}

/// Answers remembered for the current session. IN MEMORY ONLY: nothing is
/// written to disk and nothing survives the process, which is a security choice
/// (a persistent allow-list is the vector of CVE-2026-22708), not a limitation.
#[derive(Debug, Clone, Default)]
pub struct ApprovalMemory {
    inner: Arc<RwLock<HashMap<ApprovalKey, bool>>>,
}

impl ApprovalMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembered answer for this exact key, `None` when never answered.
    pub fn lookup(&self, key: &ApprovalKey) -> Option<bool> {
        match self.inner.read() {
            Ok(map) => map.get(key).copied(),
            Err(poisoned) => poisoned.into_inner().get(key).copied(),
        }
    }

    pub fn remember(&self, key: ApprovalKey, allow: bool) {
        match self.inner.write() {
            Ok(mut map) => {
                map.insert(key, allow);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(key, allow);
            }
        }
    }

    /// Snapshot for display, sorted for a stable rendering.
    pub fn entries(&self) -> Vec<ApprovalEntry> {
        let map = match self.inner.read() {
            Ok(map) => map.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let mut entries: Vec<ApprovalEntry> = map
            .into_iter()
            .map(|(key, allow)| ApprovalEntry {
                tool: key.tool.clone(),
                command: key.display(),
                allow,
            })
            .collect();
        entries.sort_by(|a, b| (&a.tool, &a.command).cmp(&(&b.tool, &b.command)));
        entries
    }

    /// Forgets everything and returns how many answers were dropped.
    pub fn clear(&self) -> usize {
        match self.inner.write() {
            Ok(mut map) => {
                let n = map.len();
                map.clear();
                n
            }
            Err(poisoned) => {
                let mut map = poisoned.into_inner();
                let n = map.len();
                map.clear();
                n
            }
        }
    }
}

/// Confirmation request presented to the user (through the `Approver`).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    /// True when the request is forced by recent untrusted content.
    pub taint_forced: bool,
    pub mode: PermissionMode,
    /// Short summary of the input (e.g. the Bash command, the written path).
    pub input_summary: String,
    /// Raw structured input: lets the frontend render a rich preview
    /// (diff for `edit`, command for `bash`) in the permission dialog.
    pub input: serde_json::Value,
    /// May the answer be remembered for the session (US-009 AC1)? The frontend
    /// only offers the option when this is true.
    pub memoizable: bool,
    /// Why remembering is unavailable, when there is a reason worth showing
    /// (US-009 AC2). `None` = the tool simply has no rememberable form.
    pub memo_refused: Option<String>,
}

/// Answer to a confirmation: what to do now, and how far that answer reaches.
///
/// Ported from Codex `ReviewDecision` (`codex-rs/protocol/src/protocol.rs:4108`).
/// Two things a boolean could not express are carried here: the user's own
/// WORDING for a refusal, which is what lets the model change approach instead
/// of retrying the same call, and the difference between refusing one action and
/// stopping the turn.
///
/// `remember` is ignored when the request is not memoizable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    /// Run it.
    Approved { remember: bool },
    /// Do not run it, and keep the turn going. `rejection` is handed to the
    /// model verbatim; `None` falls back to a generic sentence.
    Denied {
        remember: bool,
        rejection: Option<String>,
    },
    /// No answer arrived in time. Distinct from a refusal: nobody decided, so
    /// nothing is ever remembered from it.
    TimedOut,
    /// Do not run it, and stop the turn. The model is not asked to try
    /// something else, because the user is done with this turn.
    Abort,
}

impl ApprovalResponse {
    pub const ALLOW_ONCE: Self = Self::Approved { remember: false };
    pub const DENY_ONCE: Self = Self::Denied {
        remember: false,
        rejection: None,
    };
    pub const ALLOW_SESSION: Self = Self::Approved { remember: true };
    pub const DENY_SESSION: Self = Self::Denied {
        remember: true,
        rejection: None,
    };

    pub const fn once(allow: bool) -> Self {
        if allow {
            Self::ALLOW_ONCE
        } else {
            Self::DENY_ONCE
        }
    }

    /// May the call run?
    pub fn allows(&self) -> bool {
        matches!(self, Self::Approved { .. })
    }

    /// Does the answer extend to the whole session? A timeout and an abort never
    /// do: neither is a decision the user made about future calls.
    pub fn remembers(&self) -> bool {
        match self {
            Self::Approved { remember } | Self::Denied { remember, .. } => *remember,
            Self::TimedOut | Self::Abort => false,
        }
    }

    /// Should the turn stop instead of carrying on without this tool?
    pub fn aborts_turn(&self) -> bool {
        matches!(self, Self::Abort)
    }

    /// Sentence handed back to the model, `None` when the answer allows the call.
    pub fn refusal_for_model(&self, tool: &str) -> Option<String> {
        match self {
            Self::Approved { .. } => None,
            Self::Denied {
                rejection: Some(rejection),
                ..
            } if !rejection.trim().is_empty() => Some(format!(
                "action \"{tool}\" rejected by user: {}",
                rejection.trim()
            )),
            Self::Denied { .. } => Some(format!("action \"{tool}\" rejected by user")),
            Self::TimedOut => Some(format!(
                "action \"{tool}\" was not confirmed in time and did not run"
            )),
            Self::Abort => Some(format!(
                "action \"{tool}\" rejected by user, who ended the turn"
            )),
        }
    }
}

/// Interactive boundary: the pipeline delegates confirmation here. The CLI/TUI
/// provides a real implementation (prompt); tests a scripted double.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Decides the action and the scope of that decision.
    async fn approve(&self, req: &PermissionRequest) -> ApprovalResponse;
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
    async fn approve(&self, req: &PermissionRequest) -> ApprovalResponse {
        // Never remembers: an automatic answer must not build an allow-list.
        ApprovalResponse::once(!req.taint_forced || self.approve_tainted)
    }
}

/// Approver that refuses everything (fail-closed: safe default in headless mode
/// without a counterpart, or for refusal-path tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _req: &PermissionRequest) -> ApprovalResponse {
        ApprovalResponse::DENY_ONCE
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

    fn key(tokens: &[&str]) -> ApprovalKey {
        let tokens: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
        ApprovalKey::new("bash", &tokens, "/ws")
    }

    #[test]
    fn memory_matches_the_exact_token_sequence() {
        // US-008 AC1/AC2: `git status` remembered never covers `git status-x`,
        // `git status --short` nor `git push --force`.
        let memory = ApprovalMemory::new();
        memory.remember(key(&["git", "status"]), true);
        assert_eq!(memory.lookup(&key(&["git", "status"])), Some(true));
        assert_eq!(memory.lookup(&key(&["git", "status-x"])), None);
        assert_eq!(memory.lookup(&key(&["git", "status", "--short"])), None);
        assert_eq!(memory.lookup(&key(&["git", "push", "--force"])), None);
    }

    #[test]
    fn memory_is_scoped_to_the_working_directory() {
        let memory = ApprovalMemory::new();
        let tokens = vec!["ls".to_string()];
        memory.remember(ApprovalKey::new("bash", &tokens, "/a"), true);
        assert_eq!(
            memory.lookup(&ApprovalKey::new("bash", &tokens, "/b")),
            None
        );
    }

    #[test]
    fn memory_is_session_scoped_and_clearable() {
        // US-008 AC4: nothing is persisted, so a fresh memory knows nothing.
        let memory = ApprovalMemory::new();
        memory.remember(key(&["ls"]), true);
        memory.remember(key(&["rm", "-rf", "target"]), false);
        let entries = memory.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "ls");
        assert!(entries[0].allow);
        assert_eq!(memory.clear(), 2);
        assert!(memory.entries().is_empty());
        assert!(ApprovalMemory::new().entries().is_empty());
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
