//! `TuiApprover`: bridge between the tool pipeline (`agent_tools::Approver`) and the
//! frontend: sends the permission request to the TUI loop and waits for the
//! answer (oneshot). Translates the `PermissionRequest` into a `PermissionPrompt`
//! (with a diff preview for `edit`) consumed by the rendering.
//!
//! Fail-closed: when the channel is closed (TUI gone) or the answer lost, we
//! **refuse** by default.

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
