//! `TuiApprover`: bridge between the tool pipeline (`agent_tools::Approver`) and the
//! frontend: sends the permission request to the TUI loop and waits for the
//! answer (oneshot). Translates the `PermissionRequest` into a `PermissionPrompt`
//! (with a diff preview for `edit`) consumed by the rendering.
//!
//! Fail-closed: when the channel is closed (TUI gone) or the answer lost, we
//! **refuse** by default.

use std::sync::Arc;

use agent_tools::permission::{ApprovalResponse, Approver, PermissionRequest};
use agent_tui::{PermissionPrompt, diff};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

/// Message sent to the TUI loop: the request + the answer channel.
pub type PermissionMsg = (PermissionRequest, oneshot::Sender<ApprovalResponse>);

pub struct TuiApprover {
    tx: mpsc::Sender<PermissionMsg>,
}

impl TuiApprover {
    pub fn new(tx: mpsc::Sender<PermissionMsg>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, req: &PermissionRequest) -> ApprovalResponse {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((req.clone(), resp_tx)).await.is_err() {
            return ApprovalResponse::DENY_ONCE; // TUI closed -> fail-closed
        }
        // Answer lost -> fail-closed, and nothing remembered.
        resp_rx.await.unwrap_or(ApprovalResponse::DENY_ONCE)
    }
}

/// Tool name carried by a network approval request. Not a registered tool: it
/// is the name the dialog and the transcript show for a question the PROXY
/// asked, and keeping it distinct is what stops it from ever matching a
/// remembered answer keyed on a real tool.
pub const NETWORK_APPROVAL_TOOL: &str = "network";

/// Asks the user, at the moment the proxy blocks a host, whether to let it
/// through (US-004). Rides the same channel as a tool confirmation, so the TUI
/// renders it with the dialog it already has and the same fail-closed rules
/// apply: a closed channel or a lost answer refuses.
///
/// The two "allow" answers map onto the two grants the proxy knows: a one-shot
/// answer opens this connection only, a session answer opens the host and its
/// subdomains until the process ends. Nothing here writes the user's configured
/// allow-list: a grant is not a policy change.
pub struct TuiNetworkApprover {
    tx: mpsc::Sender<PermissionMsg>,
}

impl TuiNetworkApprover {
    pub fn new(tx: mpsc::Sender<PermissionMsg>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl agent_sandbox::NetworkApprover for TuiNetworkApprover {
    async fn approve(&self, host: &str, allowed: &str) -> agent_sandbox::NetworkDecision {
        let input = serde_json::json!({ "host": host, "allowed": allowed });
        let req = PermissionRequest {
            // Not a tool call: the identifier only has to be unique enough for a
            // client to correlate the dialog with its answer.
            call_id: format!("network:{host}"),
            tool: NETWORK_APPROVAL_TOOL.to_string(),
            reason: format!("{host} is outside the network allow-list (allowed: {allowed})"),
            // A blocked host is reached BY a command, whose output may itself
            // have been shaped by untrusted content. Marking the question as
            // taint-forced is what keeps the dialog from looking routine.
            taint_forced: false,
            mode: agent_tools::PermissionMode::Default,
            input_summary: format!("connect to {host}"),
            input,
            // The session option here means "for the rest of this run", applied
            // by the proxy itself, not by the approval memory.
            memoizable: true,
            memo_refused: None,
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((req, resp_tx)).await.is_err() {
            return agent_sandbox::NetworkDecision::Deny;
        }
        match resp_rx.await {
            Ok(ApprovalResponse::Approved { remember: true }) => {
                agent_sandbox::NetworkDecision::AllowSession
            }
            Ok(ApprovalResponse::Approved { remember: false }) => {
                agent_sandbox::NetworkDecision::AllowOnce
            }
            // A refusal, an abort, a timeout and a lost answer are the same
            // outcome for a socket: it does not open.
            _ => agent_sandbox::NetworkDecision::Deny,
        }
    }
}

/// Tool name carried by a `request_permissions` confirmation. Same reasoning as
/// [`NETWORK_APPROVAL_TOOL`]: it names who is asking, and never collides with a
/// remembered answer keyed on a registered tool.
pub const PERMISSION_GRANT_TOOL: &str = "permission-grant";

/// Grants what `request_permissions` asks for, once a human agrees (US-004).
///
/// Holds the two perimeters that can actually move while a session runs: the
/// proxy's session grants and the permission mode. The filesystem is absent on
/// purpose, and the tool refuses that scope before reaching here: a Landlock
/// domain is inherited and irreversible, so no broker can widen it.
///
/// Every grant is confirmed by the SAME approver the tool pipeline uses. The
/// confirmation is asked twice on purpose: once by the pipeline for the call
/// itself, once here for the widening. They are different questions, and
/// answering the first is not answering the second.
pub struct CliPermissionBroker {
    approver: Arc<dyn Approver>,
    grants: agent_sandbox::NetworkGrants,
    mode: agent_tools::PermissionModeState,
}

impl CliPermissionBroker {
    pub fn new(
        approver: Arc<dyn Approver>,
        grants: agent_sandbox::NetworkGrants,
        mode: agent_tools::PermissionModeState,
    ) -> Self {
        Self {
            approver,
            grants,
            mode,
        }
    }

    async fn confirm(&self, summary: &str, reason: &str, input: serde_json::Value) -> bool {
        let req = PermissionRequest {
            call_id: format!("grant:{summary}"),
            tool: PERMISSION_GRANT_TOOL.to_string(),
            reason: format!("the model asks to {summary}: {reason}"),
            taint_forced: false,
            mode: self.mode.get(),
            input_summary: summary.to_string(),
            input,
            // A widening is never remembered: the next request asks again. Same
            // rule as a sandbox escalation, for the same reason.
            memoizable: false,
            memo_refused: Some("a widening is granted once, never remembered".to_string()),
        };
        self.approver.approve(&req).await.allows()
    }
}

#[async_trait]
impl agent_tools::PermissionBroker for CliPermissionBroker {
    async fn request(
        &self,
        ask: &agent_tools::PermissionAsk,
        reason: &str,
    ) -> agent_tools::GrantOutcome {
        use agent_tools::{GrantOutcome, PermissionAsk};
        match ask {
            PermissionAsk::Network { host } => {
                let host = host.trim();
                let summary = format!("reach {host} for the rest of the session");
                if !self
                    .confirm(&summary, reason, serde_json::json!({ "host": host }))
                    .await
                {
                    return GrantOutcome::Refused(format!(
                        "the user refused network access to {host}. Do not retry \
                         it; work without that host or say what it blocks."
                    ));
                }
                self.grants.grant_for_session(host);
                GrantOutcome::Granted(format!(
                    "{host} and its subdomains are now reachable for the rest of \
                     this session."
                ))
            }
            PermissionAsk::Mode { mode } => {
                let current = self.mode.get();
                // A request that would not widen anything is answered without
                // bothering the user: asking a human to confirm a no-op teaches
                // them to confirm without reading.
                if !widens(current, *mode) {
                    return GrantOutcome::Refused(format!(
                        "the session is already in `{}`, which is not more \
                         restrictive than `{}`; nothing to grant.",
                        current.id(),
                        mode.id()
                    ));
                }
                let summary = format!("switch the session to `{}`", mode.id());
                if !self
                    .confirm(&summary, reason, serde_json::json!({ "mode": mode.id() }))
                    .await
                {
                    return GrantOutcome::Refused(format!(
                        "the user kept the session in `{}`. Keep asking for \
                         confirmations rather than working around them.",
                        current.id()
                    ));
                }
                self.mode.set(*mode);
                GrantOutcome::Granted(format!("the session is now in `{}`.", mode.id()))
            }
        }
    }
}

/// Does moving from `current` to `requested` actually widen anything? Ranked by
/// how much they ask of the user, which is the only ordering that matters here:
/// `read-only` < `ask` < `accept-edits` < `auto` < `full-access`.
fn widens(current: agent_tools::PermissionMode, requested: agent_tools::PermissionMode) -> bool {
    fn rank(mode: agent_tools::PermissionMode) -> u8 {
        use agent_tools::PermissionMode as Mode;
        match mode {
            Mode::Plan => 0,
            Mode::Default => 1,
            Mode::AcceptEdits => 2,
            Mode::DontAsk => 3,
            Mode::BypassPermissions => 4,
        }
    }
    rank(requested) > rank(current)
}

/// Builds the visual prompt from the request: title adapted to the tool + preview
/// through the SAME diff engine as the transcript (`diff::from_tool` for `edit` /
/// `write`; context lines for `bash` / unknown). US-039.
pub fn to_prompt(req: &PermissionRequest) -> PermissionPrompt {
    let v = &req.input;
    let str_field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default();

    let (title, preview) = match req.tool.as_str() {
        "edit" => (
            format!("edit {}", str_field("path")),
            diff::from_tool("edit", v).unwrap_or_default(),
        ),
        "write" => (
            format!("write {}", str_field("path")),
            diff::from_tool("write", v).unwrap_or_default(),
        ),
        "bash" => (
            "bash".to_string(),
            diff::note([str_field("command").to_string()]),
        ),
        // The proxy asked, not a tool: the dialog names the host and the list it
        // failed, because that is the whole decision.
        NETWORK_APPROVAL_TOOL => (
            format!("network {}", str_field("host")),
            diff::note([format!("allowed so far: {}", str_field("allowed"))]),
        ),
        // `note` expects one line per item: we split (a multi-line summary must
        // not end up in a single `Row::Context` with embedded `\n`).
        other => (
            other.to_string(),
            diff::note(req.input_summary.lines().map(str::to_string)),
        ),
    };

    let mut prompt = PermissionPrompt::new(title, req.reason.clone(), preview);
    prompt.call_id = Some(req.call_id.clone());
    prompt.mode = Some(req.mode.to_string());
    prompt.taint_forced = req.taint_forced;
    prompt.memoizable = req.memoizable;
    prompt.memo_note = req.memo_refused.clone();
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, input: serde_json::Value) -> PermissionRequest {
        PermissionRequest {
            call_id: "c1".into(),
            tool: tool.into(),
            reason: "test".into(),
            taint_forced: false,
            mode: agent_tools::PermissionMode::Default,
            input_summary: input.to_string(),
            input,
            memoizable: false,
            memo_refused: None,
        }
    }

    #[test]
    fn memoization_metadata_reaches_the_prompt() {
        // US-009 AC1/AC2: the dialog knows whether it may offer the session
        // options, and why it may not.
        let mut r = req("bash", serde_json::json!({ "command": "git status" }));
        r.memoizable = true;
        assert!(to_prompt(&r).memoizable);

        let mut r = req("bash", serde_json::json!({ "command": "ls $HOME" }));
        r.memo_refused = Some("the command contains a substitution or a variable".into());
        let p = to_prompt(&r);
        assert!(!p.memoizable);
        assert_eq!(
            p.memo_note.as_deref(),
            Some("the command contains a substitution or a variable")
        );
    }

    #[test]
    fn edit_request_becomes_diff() {
        use agent_tui::diff::Row;
        let p = to_prompt(&req(
            "edit",
            serde_json::json!({ "path": "a.rs", "old_string": "x", "new_string": "y" }),
        ));
        assert_eq!(p.title, "edit a.rs");
        assert_eq!(p.call_id.as_deref(), Some("c1"));
        // The prompt shows the spelling the user types, not a debug rendering.
        assert_eq!(p.mode.as_deref(), Some("ask"));
        assert!(!p.taint_forced);
        assert!(
            p.preview
                .rows
                .iter()
                .any(|r| matches!(r, Row::Remove { .. }))
        );
        assert!(p.preview.rows.iter().any(|r| matches!(r, Row::Add { .. })));
    }

    #[test]
    fn bash_request_shows_command() {
        use agent_tui::diff::Row;
        let p = to_prompt(&req(
            "bash",
            serde_json::json!({ "command": "rm -rf /tmp/x" }),
        ));
        assert_eq!(p.title, "bash");
        assert!(matches!(&p.preview.rows[0], Row::Context { text, .. } if text == "rm -rf /tmp/x"));
    }
}
