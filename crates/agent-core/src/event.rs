//! `AgentEvent`: THE core -> clients contract (TUI, `-p` headless).
//! Structured, serializable, NO presentation decision, NEVER ANSI
//! (ARCHITECTURE 10.1, invariant 2). Distinct from `StreamEvent` (provider -> core).

use crate::compaction::CompactKind;
use crate::error::AgentError;
use crate::message::{ToolCallId, ToolErrorKind};
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
    /// Permission request (emitted by the tool pipeline, US-013, not by the
    /// core in EP-002; present to pin down the contract).
    PermissionAsk(PermissionReq),
    EndTurn,
    Interrupted,
    Exhausted(ExhaustReason),
    Error(AgentError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

/// Output fragment produced by a tool before it ends (US-015). `chunk` is
/// external content: untrusted by construction, like the final result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputDeltaView {
    pub id: ToolCallId,
    pub chunk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultView {
    pub id: ToolCallId,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    /// Tool output = untrusted by default (taint, US-013).
    pub untrusted: bool,
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
    /// Input tokens of THIS round-trip as reported by the backend, i.e. the
    /// context actually occupied (US-002). `None` when the provider reported no
    /// usage: the measure is absent, which is not the same as zero, and a client
    /// must then display nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// Context window of the active model, when the backend declares one
    /// (US-001). `None` = unknown. The core deliberately computes NO percentage:
    /// relating one to the other is a presentation decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
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
    /// Path relative to the workspace root.
    pub path: String,
    pub change: FileChange,
    pub added_lines: u32,
    pub removed_lines: u32,
    /// Unified diff. Absent for a binary file or one larger than the diff
    /// threshold: the file stays listed, its content is not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionReq {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    pub taint_forced: bool,
    pub input_summary: String,
    pub input: serde_json::Value,
    pub mode: String,
}
