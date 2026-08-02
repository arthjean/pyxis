//! `AgentEvent`: THE core -> clients contract (TUI, `-p` headless).
//! Structured, serializable, NO presentation decision, NEVER ANSI
//! (ARCHITECTURE 10.1, invariant 2). Distinct from `StreamEvent` (provider -> core).

use crate::compaction::CompactKind;
use crate::error::AgentError;
use crate::message::{ToolCallId, ToolErrorKind};
use crate::provider::ErrorClass;
use crate::tools::{ToolExecution, ToolResultStatus, ToolResultTruncation};
use crate::transition::ExhaustReason;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The current stream was abandoned before commit (retry/recover).
    /// Clients must drop the unfinalized live deltas.
    StreamReset,
    /// Assistant text delta.
    Text(String),
    /// Reasoning delta (when the provider emits any).
    Reasoning(String),
    /// A replay rejection forced the current turn onto the byte-identical
    /// no-replay request path.
    ReasoningReplayDisabled {
        reason: String,
    },
    /// Response/header metadata that must remain observable without becoming
    /// transcript text.
    ResponseMetadata(Box<crate::provider::ResponseMetadata>),
    /// Provider-neutral response item lifecycle, with a bounded/redacted full
    /// payload for transcript consumers that understand more than text/tools.
    ResponseItem {
        phase: crate::provider::ResponseItemPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_index: Option<u64>,
        item: Box<crate::provider::ResponseItem>,
    },
    /// Additive provider event with a bounded and redacted payload.
    ProviderExtension(crate::provider::ProviderExtension),
    /// The backend served a response item the provider adapter does not map, so
    /// its content never reached the transcript. Reported rather than dropped:
    /// silence here reads as "the model produced nothing", which is false.
    /// Carries the wire tag plus an optional bounded, sanitized provider copy.
    UnmappedResponseItem {
        item_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<crate::provider::ProviderExtension>,
    },
    /// A provider reopening planned from the single sampling-scoped attempt
    /// budget. It carries identifiers and allow-listed classifications only.
    RetryScheduled(RetryScheduledView),
    /// Lifecycle of the one credential recovery permitted for a sampling.
    CredentialRefresh(CredentialRefreshView),
    /// A tool is about to run.
    ToolCall(ToolCallView),
    /// Output fragment of a tool still running (US-015). Purely
    /// informational: the final `ToolResult` stays the only transcript source,
    /// and a client that ignores this variant keeps the previous behavior.
    ToolOutputDelta(ToolOutputDeltaView),
    /// Tool result (the taint lives in the view-model, US-013).
    ToolResult(ToolResultView),
    /// A compaction just happened.
    Compacted(CompactKind),
    /// A model round-trip just ended (US-017). Emitted after every complete
    /// provider response, whether it closes the turn or chains into tools.
    /// Purely informational: a client that ignores it keeps the previous
    /// behavior.
    ModelTurn(ModelTurnView),
    /// Aggregated diff of the files modified during the turn (US-018). Emitted by
    /// the CLIENT at the end-of-turn boundary, not by the loop: computing a diff
    /// means reading the disk, which the core does not do (invariant 1). Never
    /// emitted when nothing changed.
    TurnDiff(TurnDiffView),
    /// Subscription quota state reported by the backend (US-003). Emitted only
    /// when the provider served something usable. Purely informational: a client
    /// that ignores it keeps the previous behavior.
    Quota(crate::quota::QuotaSnapshot),
    /// Task plan published by the model (US-009). Purely informational: a client
    /// that ignores this variant keeps the previous behavior, and the plan never
    /// drives the loop.
    Plan(PlanView),
    /// Permission request (emitted by the tool pipeline, US-013, not by the
    /// core in EP-002; present to pin down the contract).
    PermissionAsk(PermissionReq),
    /// A hook ran. Hooks decide whether a tool call happens at all, so a run
    /// that blocks one has to be visible: without this event a refusal reads as
    /// the agent silently choosing not to act.
    Hook(HookRunView),
    EndTurn,
    Interrupted(InterruptedView),
    Exhausted(ExhaustReason),
    Error(AgentError),
}

/// Why a turn stopped short, and what it cost before stopping.
///
/// Ported from Codex `TurnAbortedEvent` (`codex-rs/protocol/src/protocol.rs:4200`).
/// Only the causes Pyxis can actually reach are modelled: budget exhaustion
/// already travels as [`AgentEvent::Exhausted`], and a steer replaces a sampling
/// without aborting the turn, so neither has a variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedView {
    pub reason: InterruptReason,
    /// Epoch ms, from the injected clock. Absent when the turn start was not
    /// observed, which a client must render as "unknown", not as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Tool calls that were left without a result and had one written for them
    /// during reconciliation. Non-zero means the model may have half-applied
    /// effects it never saw reported.
    pub reconciled_tool_calls: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptReason {
    /// The client signalled cooperative cancellation.
    Cancelled,
    /// The user refused a tool call and ended the turn with that refusal.
    ToolAborted,
}

impl InterruptedView {
    /// A cancellation with no timing observed. The shape a client builds when it
    /// aborts a turn locally and has nothing to report but the cause.
    pub fn cancelled() -> Self {
        Self {
            reason: InterruptReason::Cancelled,
            started_at_ms: None,
            completed_at_ms: None,
            duration_ms: None,
            reconciled_tool_calls: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryScheduledView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// 1-based model sampling within the turn.
    pub step: u32,
    /// 1-based ordinal of the provider opening that will follow.
    pub ordinal: u32,
    pub max_attempts: u32,
    pub cause: ErrorClass,
    pub delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    pub prompt_fingerprint: String,
    pub model_runtime_fingerprint: String,
    pub tool_plan_fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRefreshOutcome {
    Started,
    Succeeded,
    Permanent,
    Transient,
    Unavailable,
    /// Recovery failed without a typed credential cause.
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRefreshView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub step: u32,
    /// Provider opening whose 401 triggered the recovery.
    pub attempt_ordinal: u32,
    pub outcome: CredentialRefreshOutcome,
}

/// What KIND of act a tool call is, as the pipeline knows it.
///
/// Codex models these as separate event families: `ExecCommandBegin/End`
/// (`codex-rs/protocol/src/protocol.rs:3518`), `PatchApplyBegin/End` (`:3678`)
/// and `McpToolCallBegin/End` (`:2411`). Pyxis keeps ONE call event and
/// qualifies it, which gives clients the same information without a family per
/// tool. The qualification comes from the tool itself: deriving it from the
/// name, as the TUI used to, misreads any MCP tool that happens to be called
/// `bash` and has to be reimplemented by every client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallKind {
    /// Nothing more specific: a plain function call.
    #[default]
    Other,
    /// Runs a command. `command` is what will actually be executed.
    Exec {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Edits files. The changed paths, when they are known before the run.
    Patch {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// Served by an MCP server rather than built in.
    Mcp { server: String, tool: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
    /// Nature of the call. Defaults to `Other`, so a client that ignores it
    /// behaves exactly as before.
    #[serde(default)]
    pub kind: ToolCallKind,
}

/// Which of a process's two streams produced a fragment. Ported from Codex
/// `ExecOutputStream` (`codex-rs/protocol/src/protocol.rs:3615`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Output fragment produced by a tool before it ends (US-015). `chunk` is
/// external content: untrusted by construction, like the final result.
///
/// The bytes are carried RAW, exactly as the process wrote them. Decoding them
/// to text is a presentation decision (invariant 1): a command may emit a
/// binary payload or a partial UTF-8 sequence split across two fragments, and
/// replacing those with U+FFFD inside the core would destroy them for every
/// client at once. [`ToolOutputDeltaView::chunk_lossy`] is there for the clients
/// that just want a string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputDeltaView {
    pub id: ToolCallId,
    /// Stream of origin, so a client can tell a diagnostic from a result.
    pub stream: OutputStream,
    #[serde(with = "base64_bytes")]
    pub chunk: Vec<u8>,
}

impl ToolOutputDeltaView {
    /// The fragment as text, invalid sequences replaced. For display only.
    pub fn chunk_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.chunk)
    }
}

/// Base64 on the wire: an event is JSON, and a raw byte array would otherwise
/// serialize as a list of numbers several times its size.
mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultView {
    pub id: ToolCallId,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolResultStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    /// Tool output = untrusted by default (taint, US-013).
    pub untrusted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ToolResultTruncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ToolExecution>,
}

impl ToolResultView {
    pub fn from_model(result: &crate::tools::ModelToolResult) -> Self {
        Self {
            id: result.id.clone(),
            content: result.content.clone(),
            status: Some(result.status),
            structured_content: result.structured_content.clone(),
            is_error: result.is_error,
            error_kind: result.error_kind,
            untrusted: result.untrusted,
            duration_ms: result.duration_ms,
            truncation: result.truncation.clone(),
            execution: result.execution.clone(),
        }
    }
}

/// End of a model round-trip (US-017). The counters are CUMULATED since the
/// start of the run: they are the ones driving the budget, so real when the
/// provider reports its `usage`, locally estimated otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelTurnView {
    /// 1-based index of the model turn that just ended.
    pub index: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Counters of THIS round-trip as the backend reported them. `None` when it
    /// reported no usage: the two fields above then carry a local estimate, and
    /// conflating the two would make a cost read as measured when it is guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<crate::provider::TokenUsage>,
    /// Element-wise sum of every REPORTED round-trip since the start of the run.
    /// It is what makes cache efficiency and reasoning share computable, which
    /// the two flat totals cannot express. Mirrors Codex `TokenUsageInfo`
    /// (`codex-rs/protocol/src/protocol.rs:2075`).
    #[serde(default)]
    pub total_usage: crate::provider::TokenUsage,
    /// Input tokens of THIS round-trip as reported by the backend, i.e. the
    /// context actually occupied (US-002). `None` when the provider reported no
    /// usage: the measure is absent, which is not the same as zero, and a client
    /// must then display nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Context window of the active model, when the backend declares one
    /// (US-001). `None` = unknown. The core deliberately computes NO percentage:
    /// relating one to the other is a presentation decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Descriptor threshold that drives proactive compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u32>,
    /// Local estimate of the same input, produced only when the calibration
    /// probe is enabled (`RunConfig::usage_probe`). Exists to be compared to
    /// `context_tokens`; `None` in the nominal case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_context_tokens: Option<u32>,
}

/// Aggregated diff of a turn (US-018). An empty one is never emitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDiffView {
    pub files: Vec<FileDiffView>,
}

impl TurnDiffView {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Lines added then removed, across all files.
    pub fn totals(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(added, removed), file| {
            (
                added.saturating_add(file.added_lines),
                removed.saturating_add(file.removed_lines),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffView {
    /// Path relative to the workspace root. For a rename, the DESTINATION.
    pub path: String,
    pub change: FileChange,
    pub added_lines: u32,
    pub removed_lines: u32,
    /// Unified diff. Absent for a binary file or one larger than the diff
    /// threshold: the file stays listed, its content is not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
    /// Where the file came from, when this change is a rename. Mirrors Codex
    /// `FileChange::Update { move_path }`
    /// (`codex-rs/protocol/src/protocol.rs:4189`). Without it a rename reads as
    /// an unrelated delete plus an unrelated create, which is what a reviewer
    /// must not be shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    /// Same content, new path. A rename that also changed the content stays
    /// reported as a delete plus an add: pairing those would require a
    /// similarity threshold, which is a judgement call this layer does not make.
    Renamed,
}

/// Plan of the current task (US-009), as the model states it. Pure data: the
/// core neither validates it (the tool does) nor renders it (the client does).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    /// Optional rationale for the update. Absent = the steps speak for themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStatus,
}

/// The three states of a plan step, taken from Codex (`plan_spec.rs:7`). At most
/// one step is `InProgress`, an invariant the tool enforces before the event
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// How a hook run ended. Ported from Codex `HookRunStatus`
/// (`codex-rs/protocol/src/protocol.rs:1556`), minus the states Pyxis cannot
/// reach: hooks run to completion before the pipeline continues, so there is no
/// `Running` to report, and nothing here can stop the session outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRunStatus {
    /// Ran, and raised no objection.
    Completed,
    /// Refused the action, or forced a confirmation for it.
    Blocked,
    /// Could not run, or exited on an error. Fail-closed on a gating event.
    Failed,
}

/// One hook run, as the clients see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRunView {
    /// Event name of the reference contract (`PreToolUse`, `SessionStart`, ...).
    pub event: String,
    /// Tool the run concerns, absent on a lifecycle event, which names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub status: HookRunStatus,
    /// Why it blocked or failed. Absent on a plain completion, which has nothing
    /// to explain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionReq {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    pub taint_forced: bool,
    pub input_summary: String,
    pub input: serde_json::Value,
    /// Mode the request was raised under, as a value. It used to be a
    /// debug-formatted string, which forced every consumer to parse prose to
    /// recover one of five known values.
    pub mode: crate::permission::PermissionMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ModelToolResult, ToolExecution, ToolResultStatus};

    #[test]
    fn public_tool_result_event_preserves_typed_terminal_metadata() {
        let mut result = ModelToolResult::new("call".into(), "timed out".into(), true, true, None);
        result.status = ToolResultStatus::TimedOut;
        result.duration_ms = Some(125);
        result.execution = Some(ToolExecution {
            timed_out: true,
            ..ToolExecution::default()
        });

        let json = serde_json::to_value(ToolResultView::from_model(&result)).unwrap();

        assert_eq!(json["status"], "timed_out");
        assert_eq!(json["duration_ms"], 125);
        assert_eq!(json["execution"]["timed_out"], true);
    }
}
