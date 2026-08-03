//! Canonical message types (Anthropic-like format, content blocks: see
//! PROVIDERS 1.1). `agent-core` is the crate of canonical types: the whole
//! system (provider, session, tools) only knows these types.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::tools::{ModelToolResult, ToolExecution, ToolResultStatus, ToolResultTruncation};

pub type ToolCallId = String;

/// US-002: content of the result written for a tool call left unanswered
/// after an interruption. It serves two audiences: the backend, which rejects
/// with a 400 any `tool_use` without a matching `tool_result`, and the model,
/// which must read in the history that the user interrupted the execution.
///
/// The message carries the CONSEQUENCE and not only the fact: an interrupted
/// tool may have written half of its effects. Without that detail, the model
/// assumes by default that nothing happened and resumes on a wrong state (same
/// reasoning as Codex CLI's `<turn_aborted>`).
pub const INTERRUPTED_TOOL_RESULT: &str = "Interrupted by the user before this tool call \
     completed. No result is available: the tool may have partially executed and any process \
     it started may still be running.";

/// Transcript tool calls left WITHOUT a result, in order of appearance.
///
/// This is the single definition of the `tool_use` <-> `tool_result` pairing:
/// the loop uses it to reconcile before persisting (US-002) and the provider
/// adapter to refuse emitting an orphan call (US-003). A given id is
/// returned only once: the repair cannot produce a duplicate.
pub fn unanswered_tool_calls(messages: &[Message]) -> Vec<ToolCallId> {
    let mut answered: HashSet<&str> = HashSet::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
            answered.insert(tool_use_id.as_str());
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut pending = Vec::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolUse { id, .. } = block
            && !answered.contains(id.as_str())
            && seen.insert(id.as_str())
        {
            pending.push(id.clone());
        }
    }
    pending
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    UnknownTool,
    Parse,
    Validation,
    OutsideWorkspace,
    Io,
    Rejected,
    PermissionDenied,
    SandboxDenied,
    Timeout,
    Cancelled,
    Semantic,
}

impl ToolErrorKind {
    /// Canonical textual form, identical to the `snake_case` serde
    /// representation above. Surfaces that need the category as a plain string
    /// (a JavaScript `Error.kind`, a log field) read it HERE rather than
    /// hand-rolling a second projection of the same enum.
    pub const fn label(self) -> &'static str {
        match self {
            Self::UnknownTool => "unknown_tool",
            Self::Parse => "parse",
            Self::Validation => "validation",
            Self::OutsideWorkspace => "outside_workspace",
            Self::Io => "io",
            Self::Rejected => "rejected",
            Self::PermissionDenied => "permission_denied",
            Self::SandboxDenied => "sandbox_denied",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Semantic => "semantic",
        }
    }
}

/// How a tool call carries its input. Provider-neutral: a `Text` call is what
/// the Responses API happens to call a custom tool call, but the core only
/// knows "this input is text, not JSON arguments".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    #[default]
    Json,
    Text,
}

impl ToolCallFormat {
    pub const fn is_json(&self) -> bool {
        matches!(self, Self::Json)
    }

    pub const fn is_text(&self) -> bool {
        matches!(self, Self::Text)
    }
}

const fn default_untrusted() -> bool {
    true
}

const fn default_summary_source_untrusted() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Canonical content block (`text` / `thinking` / `tool_use` / `tool_result` /
/// `image`). On `full` compaction, `Image` blocks are stripped (section 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        /// JSON arguments for a `Json` call, the raw text for a `Text` one.
        input: serde_json::Value,
        /// Absent from JSONL written before freeform tools existed, which is
        /// exactly the `Json` case: the field is additive and read tolerantly.
        #[serde(default, skip_serializing_if = "ToolCallFormat::is_json")]
        format: ToolCallFormat,
    },
    ToolResult {
        tool_use_id: ToolCallId,
        content: String,
        /// New canonical metadata is additive so historical JSONL stays
        /// readable. Fresh results always set `status`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolResultStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_content: Option<serde_json::Value>,
        /// Tool result untrusted by default. Older JSONL without this field
        /// are read back fail-closed.
        #[serde(default = "default_untrusted")]
        untrusted: bool,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<ToolErrorKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncation: Option<ToolResultTruncation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution: Option<ToolExecution>,
        /// Images the tool read, attached to the RESULT rather than injected as
        /// a separate user message. The Responses API accepts a structured
        /// `function_call_output.output` for exactly this
        /// (`codex-rs/protocol/src/models.rs:1843`); splitting them out inserted
        /// a phantom user turn between two tool turns, which shifted every
        /// downstream count of user messages.
        ///
        /// Additive: JSONL written before this field is read back with no image,
        /// which is what those transcripts actually held.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::tools::ToolImage>,
    },
    Image {
        media_type: String,
        data: String,
    },
    /// ENCRYPTED reasoning item from the Codex backend (US-031, isolated replay).
    /// Captured when `reasoning_replay` is active, to re-emit the `rs`/`fc` pair;
    /// DROPPED on compaction (protocol constraint). The `encrypted_content` is
    /// opaque (never logged nor displayed).
    EncryptedReasoning {
        id: String,
        encrypted_content: String,
    },
    /// Typed compaction summary. Older logs used a prefixed text user message;
    /// this variant avoids collisions with a genuine user prompt.
    Summary {
        text: String,
        /// True if the summary derives at least partly from tool output or from
        /// summaries whose trust level is unknown. Older JSONL read back without
        /// this field fail safe.
        #[serde(default = "default_summary_source_untrusted")]
        source_untrusted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::single(Role::System, ContentBlock::Text { text: text.into() })
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::single(Role::User, ContentBlock::Text { text: text.into() })
    }
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::single(Role::Assistant, ContentBlock::Text { text: text.into() })
    }
    pub fn tool_result(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::tool_result_with_trust(id, content, is_error, true)
    }

    pub fn tool_result_with_trust(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
        untrusted: bool,
    ) -> Self {
        Self::tool_result_with_metadata(id, content, is_error, untrusted, None)
    }

    pub fn tool_result_with_metadata(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
        untrusted: bool,
        error_kind: Option<ToolErrorKind>,
    ) -> Self {
        let result =
            ModelToolResult::new(id.into(), content.into(), is_error, untrusted, error_kind);
        Self::tool_result_from_model(&result)
    }

    pub fn tool_result_from_model(result: &ModelToolResult) -> Self {
        Self::single(
            Role::Tool,
            ContentBlock::ToolResult {
                tool_use_id: result.id.clone(),
                content: result.content.clone(),
                status: Some(result.status),
                structured_content: result.structured_content.clone(),
                untrusted: result.untrusted,
                is_error: result.is_error,
                error_kind: result.error_kind,
                duration_ms: result.duration_ms,
                truncation: result.truncation.clone(),
                execution: result.execution.clone(),
                images: result.images.clone(),
            },
        )
    }

    fn single(role: Role, block: ContentBlock) -> Self {
        Self {
            role,
            content: vec![block],
        }
    }

    /// Concatenates every `Text` block (useful for summaries / display).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            match b {
                ContentBlock::Text { text } | ContentBlock::Summary { text, .. } => {
                    out.push_str(text);
                }
                _ => {}
            }
        }
        out
    }

    /// Does this message carry at least one `tool_result`? (target of the
    /// microcompact: we prune the oldest ones first.)
    pub fn is_tool_result(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }

    pub fn has_images(&self) -> bool {
        self.content.iter().any(|b| match b {
            ContentBlock::Image { .. } => true,
            ContentBlock::ToolResult { images, .. } => !images.is_empty(),
            _ => false,
        })
    }

    /// Removes every image (full compaction: we do not pay for vision twice).
    /// Returns how many were removed, counting those carried by a tool result:
    /// leaving those behind would keep the exact cost this compaction exists to
    /// drop.
    pub fn strip_images(&mut self) -> usize {
        let before = self.content.len();
        self.content
            .retain(|b| !matches!(b, ContentBlock::Image { .. }));
        let mut removed = before - self.content.len();
        for block in &mut self.content {
            if let ContentBlock::ToolResult { images, .. } = block {
                removed += images.len();
                images.clear();
            }
        }
        removed
    }

    /// Does the message carry content that must stay treated as untrusted
    /// by the next tool or compaction decisions?
    pub fn carries_untrusted_content(&self) -> bool {
        self.content.iter().any(ContentBlock::carries_untrusted_content)
            || (self.role == Role::User
                && self
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Previous conversation summary]\n"))))
    }

    pub fn validate(&self) -> Result<(), MessageValidationError> {
        if self.content.is_empty() {
            return Err(MessageValidationError::EmptyContent);
        }
        for block in &self.content {
            match (self.role, block) {
                (Role::System, ContentBlock::Text { .. }) => {}
                (
                    Role::User,
                    ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Summary { .. },
                ) => {}
                (
                    Role::Assistant,
                    ContentBlock::Text { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::EncryptedReasoning { .. },
                ) => {}
                (Role::Tool, ContentBlock::ToolResult { .. }) => {}
                _ => {
                    return Err(MessageValidationError::InvalidBlockForRole {
                        role: self.role,
                        block_type: block.kind(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl ContentBlock {
    /// Call whose arguments are JSON validated against a tool schema.
    pub fn tool_use(
        id: impl Into<ToolCallId>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
            format: ToolCallFormat::Json,
        }
    }

    /// Call whose input is raw text (freeform tool). The text is stored as a
    /// JSON string so one transcript shape carries both formats.
    pub fn custom_tool_use(
        id: impl Into<ToolCallId>,
        name: impl Into<String>,
        input: impl Into<String>,
    ) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::Value::String(input.into()),
            format: ToolCallFormat::Text,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Thinking { .. } => "thinking",
            ContentBlock::ToolUse { .. } => "tool_use",
            ContentBlock::ToolResult { .. } => "tool_result",
            ContentBlock::Image { .. } => "image",
            ContentBlock::EncryptedReasoning { .. } => "encrypted_reasoning",
            ContentBlock::Summary { .. } => "summary",
        }
    }

    pub fn carries_untrusted_content(&self) -> bool {
        match self {
            ContentBlock::ToolResult { untrusted, .. } => *untrusted,
            ContentBlock::Summary {
                source_untrusted, ..
            } => *source_untrusted,
            _ => false,
        }
    }
}

/// Tells whether the recent tail of the transcript still holds untrusted content.
/// Used at resume to re-seed the permission taint without scanning the whole log.
pub fn recent_untrusted_content(messages: &[Message], window_messages: usize) -> bool {
    messages
        .iter()
        .rev()
        .take(window_messages)
        .any(Message::carries_untrusted_content)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageValidationError {
    #[error("message content is empty")]
    EmptyContent,
    #[error("block {block_type} is invalid for role {role:?}")]
    InvalidBlockForRole {
        role: Role,
        block_type: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_concatenates_text_blocks_only() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Thinking {
                    text: "ignored".into(),
                },
                ContentBlock::Text { text: "b".into() },
            ],
        };
        assert_eq!(m.text(), "ab");
    }

    // US-031: the EncryptedReasoning variant serializes to a snake_case tag and
    // round-trips (JSONL back-compat: additive variant, existing sessions intact).
    #[test]
    fn encrypted_reasoning_serde_roundtrip() {
        let b = ContentBlock::EncryptedReasoning {
            id: "rs_1".into(),
            encrypted_content: "OPAQUE".into(),
        };
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"type\":\"encrypted_reasoning\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn strip_images_removes_image_blocks() {
        let mut m = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "xxxx".into(),
                },
            ],
        };
        assert!(m.has_images());
        assert_eq!(m.strip_images(), 1);
        assert!(!m.has_images());
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn tool_result_detection() {
        assert!(Message::tool_result("id1", "out", false).is_tool_result());
        assert!(!Message::user("hi").is_tool_result());
    }

    // US-002/US-003: the pairing returns the calls WITHOUT a result, in order,
    // exactly once each. That is what guarantees "no duplicate, none forgotten".
    #[test]
    fn unanswered_tool_calls_reports_each_pending_call_once_in_order() {
        let messages = vec![
            Message::assistant(vec![
                ContentBlock::tool_use("c1", "bash", serde_json::json!({})),
                ContentBlock::tool_use("c2", "bash", serde_json::json!({})),
            ]),
            Message::tool_result("c2", "done", false),
            // Defensive repetition of the same call: a single result must follow.
            Message::assistant(vec![ContentBlock::tool_use(
                "c1",
                "bash",
                serde_json::json!({}),
            )]),
        ];
        assert_eq!(unanswered_tool_calls(&messages), vec!["c1".to_string()]);
    }

    #[test]
    fn unanswered_tool_calls_is_empty_on_a_healthy_transcript() {
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![ContentBlock::tool_use(
                "c1",
                "bash",
                serde_json::json!({}),
            )]),
            Message::tool_result("c1", "out", false),
        ];
        assert!(unanswered_tool_calls(&messages).is_empty());
    }

    #[test]
    fn tool_result_untrusted_defaults_to_true_for_old_json() {
        let json = r#"{"type":"tool_result","tool_use_id":"c1","content":"out","is_error":false}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(
            block,
            ContentBlock::ToolResult {
                untrusted: true,
                ..
            }
        ));
    }

    #[test]
    fn summary_source_untrusted_defaults_to_true_for_old_json() {
        let json = r#"{"type":"summary","text":"summary"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(
            block,
            ContentBlock::Summary {
                source_untrusted: true,
                ..
            }
        ));
    }

    #[test]
    fn recent_untrusted_content_checks_tail() {
        let clean = Message::user("ok");
        let tainted = Message::tool_result("c1", "evil", false);
        assert!(recent_untrusted_content(&[clean.clone(), tainted], 2));
        assert!(!recent_untrusted_content(
            &[
                Message::tool_result_with_trust("c1", "ok", false, false),
                clean
            ],
            1
        ));
    }

    #[test]
    fn validation_rejects_tool_result_in_assistant_message() {
        let msg = Message::assistant(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "out".into(),
            status: None,
            structured_content: None,
            untrusted: true,
            is_error: false,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
            images: Vec::new(),
        }]);
        assert!(msg.validate().is_err());
    }

    #[test]
    fn typed_tool_result_metadata_survives_canonical_serde() {
        let mut result = ModelToolResult::new(
            "c1".into(),
            "bounded output".into(),
            true,
            true,
            Some(ToolErrorKind::Timeout),
        );
        result.status = ToolResultStatus::TimedOut;
        result.structured_content = Some(serde_json::json!({"partial": true}));
        result.duration_ms = Some(42);
        result.truncation = Some(ToolResultTruncation {
            original_bytes: 100,
            kept_bytes: 14,
            strategy: crate::tools::TruncationStrategy::Tail,
            continuation_hint: "continue".into(),
        });
        result.execution = Some(ToolExecution {
            timed_out: true,
            stderr_tail: Some("last stderr".into()),
            ..ToolExecution::default()
        });

        let message = Message::tool_result_from_model(&result);
        let json = serde_json::to_string(&message).unwrap();
        let resumed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(resumed, message);
        assert!(matches!(
            &resumed.content[0],
            ContentBlock::ToolResult {
                status: Some(ToolResultStatus::TimedOut),
                duration_ms: Some(42),
                execution: Some(ToolExecution {
                    timed_out: true,
                    ..
                }),
                ..
            }
        ));
    }
}
