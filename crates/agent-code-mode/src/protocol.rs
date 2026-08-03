//! Vocabulary of a Code Mode session: identifiers, requests, cell states,
//! outputs and typed failures. Nothing here knows about JavaScript, V8 or the
//! Responses wire; the engine (`CellEngine`) and the tools (`agent-tools`) are
//! the two sides that project it.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use agent_core::provider::ToolSpec;
use serde::{Deserialize, Serialize};

/// Default time `exec` waits for a cell to finish before yielding what it has.
/// Same value as the Codex baseline (`DEFAULT_EXEC_YIELD_TIME_MS`).
pub const DEFAULT_YIELD_TIME: Duration = Duration::from_millis(10_000);
/// Grace given to a cell asked to stop before its state is forced to terminal.
/// Aligned on the PRD reliability budget for process groups.
pub const DEFAULT_TERMINATE_GRACE: Duration = Duration::from_millis(2_000);
/// Ceiling of the output ONE cell result may carry (PRD resource bounds).
pub const MAX_CELL_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
/// Cells one session may keep alive at once. A runtime constant, like every
/// other orchestration bound of Pyxis: no configuration key is added for it.
pub const MAX_ACTIVE_CELLS: usize = 8;

/// Identifier of a Code Mode session. One session per thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Separator between the session part and the ordinal part of a `CellId`. A
/// cell therefore NAMES its session, which is what lets a session refuse a cell
/// belonging to another one before reading any of its output (AC4).
const CELL_SEPARATOR: char = '#';

/// Identifier of one cell, stable for the whole life of that cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellId(String);

impl CellId {
    /// Builds the identifier of the `ordinal`-th cell of `session`.
    pub fn new(session: &SessionId, ordinal: u64) -> Self {
        Self(format!("{}{CELL_SEPARATOR}{ordinal}", session.as_str()))
    }

    /// Reads an identifier the model sent back. No validation beyond
    /// non-emptiness: the session that owns it is the one that judges it.
    pub fn parse(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.trim().is_empty()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Session part of the identifier, or `None` when it has no separator.
    pub fn session(&self) -> Option<&str> {
        self.0
            .rsplit_once(CELL_SEPARATOR)
            .map(|(session, _)| session)
    }

    pub fn belongs_to(&self, session: &SessionId) -> bool {
        self.session() == Some(session.as_str())
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a cell is in its life. `Completed` and `Failed` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    Running,
    Yielded,
    Completed,
    Failed,
}

impl CellState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Textual label, so a surface can show the state without relying on
    /// colour alone (PRD accessibility requirement).
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Yielded => "yielded",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Why a cell failed. The categories mirror what the US-005 spike proved
/// distinguishable; collapsing them would hide which quota fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellFailureKind {
    /// The script threw or failed to compile.
    Script,
    /// The CPU budget of the cell expired and it was terminated.
    CpuBudget,
    /// The isolate approached its heap ceiling.
    HeapLimit,
    /// A native allocation was refused by the memory budget.
    NativeMemory,
    /// The cell was stopped from outside: `terminate`, `shutdown`, a turn
    /// interruption, or a crash whose cell is repaired on resume.
    Interrupted,
    /// The engine died or never confirmed the outcome.
    EngineLost,
}

impl CellFailureKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Script => "script_error",
            Self::CpuBudget => "cpu_budget",
            Self::HeapLimit => "heap_limit",
            Self::NativeMemory => "native_memory",
            Self::Interrupted => "interrupted",
            Self::EngineLost => "engine_lost",
        }
    }
}

/// A terminal failure with the message the model and the human both read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellFailure {
    pub kind: CellFailureKind,
    pub message: String,
}

impl CellFailure {
    pub fn new(kind: CellFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn interrupted(message: impl Into<String>) -> Self {
        Self::new(CellFailureKind::Interrupted, message)
    }
}

impl fmt::Display for CellFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.kind.label(), self.message)
    }
}

/// Rendering hint carried by an image item, as the Responses wire spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

/// One piece of output a cell produced, in the order it produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    Audio {
        audio_url: String,
    },
}

impl OutputItem {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Bytes this item costs against the per-result ceiling.
    pub fn weight(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Image { image_url, .. } => image_url.len(),
            Self::Audio { audio_url } => audio_url.len(),
        }
    }
}

/// A nested tool as the cell sees it. Built from the canonical `ToolSpec` so
/// Code Mode never grows a second description of the same tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedTool {
    pub name: String,
    /// Identifier the JavaScript side exposes on the `tools` object.
    pub binding: String,
    pub description: String,
    pub freeform: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

impl NestedTool {
    /// Projects a whole tool set, allocating a UNIQUE binding to each tool.
    ///
    /// Uniqueness is a property of the SET, not of one name: `ToolSpec` accepts
    /// both `-` and `_`, and `agent-mcp` keeps the dash when it composes
    /// `mcp__server__tool`, so two legitimately distinct names can normalize to
    /// the same JavaScript identifier. Left unresolved, the second binding
    /// silently overwrites the first on the `tools` object and the model calls
    /// one tool believing it called the other. Same shape as
    /// `agent_mcp::naming::qualified_name`, which already carries a `taken` set
    /// for exactly this reason.
    pub fn catalog(specs: &[ToolSpec]) -> Vec<Self> {
        let mut taken: HashSet<String> = HashSet::with_capacity(specs.len());
        specs
            .iter()
            .map(|spec| {
                let mut tool = Self::from_spec(spec);
                let base = std::mem::take(&mut tool.binding);
                let mut candidate = base.clone();
                // Bounded: `taken` is finite, so a free name is reached in at
                // most `taken.len() + 1` attempts.
                let mut attempt = 1_usize;
                while !taken.insert(candidate.clone()) {
                    candidate = format!("{base}_{attempt}");
                    attempt += 1;
                }
                tool.binding = candidate;
                tool
            })
            .collect()
    }

    /// One tool in isolation. Its binding is NOT unique on its own: only
    /// `catalog` can promise that. Kept private so no caller can build half a
    /// catalog by hand.
    fn from_spec(spec: &ToolSpec) -> Self {
        Self {
            name: spec.name.clone(),
            binding: normalize_binding(&spec.name),
            description: spec.description.clone(),
            freeform: spec.is_freeform(),
            input_schema: spec.input_schema().cloned(),
            output_schema: spec.output_schema().cloned(),
        }
    }
}

/// Turns a tool name into a JavaScript identifier. A name is already limited to
/// `[A-Za-z0-9_-]` by `ToolSpec::validate`, so only `-` and a leading digit can
/// make it invalid: a `-` becomes `_` in place, and only a leading digit needs
/// a prefix, since a substituted character is already a valid first character.
pub fn normalize_binding(name: &str) -> String {
    let mut binding = String::with_capacity(name.len() + 1);
    for (index, character) in name.chars().enumerate() {
        if index == 0 && character.is_ascii_digit() {
            binding.push('_');
        }
        let valid = character.is_ascii_alphanumeric() || character == '_';
        binding.push(if valid { character } else { '_' });
    }
    if binding.is_empty() {
        binding.push('_');
    }
    binding
}

/// What `exec` asks the session to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecuteRequest {
    /// Identifier of the model tool call this cell answers. Kept so a result
    /// can be correlated back to the call that asked for it.
    pub call_id: String,
    pub source: String,
    pub tools: Vec<NestedTool>,
    pub yield_time: Duration,
    pub max_output_bytes: usize,
}

impl ExecuteRequest {
    pub fn new(call_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            source: source.into(),
            tools: Vec::new(),
            yield_time: DEFAULT_YIELD_TIME,
            max_output_bytes: MAX_CELL_OUTPUT_BYTES,
        }
    }

    pub fn with_tools(mut self, tools: Vec<NestedTool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_yield_time(mut self, yield_time: Duration) -> Self {
        self.yield_time = yield_time;
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }
}

/// What `wait` asks the session to do with an existing cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitRequest {
    pub cell_id: CellId,
    pub yield_time: Duration,
    /// `true` stops the cell instead of waiting for more output.
    pub terminate: bool,
}

impl WaitRequest {
    pub fn new(cell_id: CellId) -> Self {
        Self {
            cell_id,
            yield_time: DEFAULT_YIELD_TIME,
            terminate: false,
        }
    }

    pub fn terminating(cell_id: CellId) -> Self {
        Self {
            cell_id,
            yield_time: DEFAULT_YIELD_TIME,
            terminate: true,
        }
    }

    pub fn with_yield_time(mut self, yield_time: Duration) -> Self {
        self.yield_time = yield_time;
        self
    }
}

/// Answer of one `exec` or `wait` call. Every variant carries ONLY the output
/// produced since the previous yield of that cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum RuntimeResponse {
    /// The cell is still alive and can be resumed with `wait`.
    Yielded {
        cell_id: CellId,
        items: Vec<OutputItem>,
        /// Bytes dropped because the result ceiling was reached.
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_bytes: usize,
    },
    /// The cell was stopped from outside.
    Terminated {
        cell_id: CellId,
        items: Vec<OutputItem>,
        failure: CellFailure,
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_bytes: usize,
    },
    /// The cell reached its own end, with or without an error.
    Finished {
        cell_id: CellId,
        items: Vec<OutputItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<CellFailure>,
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_bytes: usize,
    },
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RuntimeResponse {
    pub fn cell_id(&self) -> &CellId {
        match self {
            Self::Yielded { cell_id, .. }
            | Self::Terminated { cell_id, .. }
            | Self::Finished { cell_id, .. } => cell_id,
        }
    }

    pub fn items(&self) -> &[OutputItem] {
        match self {
            Self::Yielded { items, .. }
            | Self::Terminated { items, .. }
            | Self::Finished { items, .. } => items,
        }
    }

    pub fn state(&self) -> CellState {
        match self {
            Self::Yielded { .. } => CellState::Yielded,
            Self::Terminated { .. } => CellState::Failed,
            Self::Finished { failure, .. } => match failure {
                Some(_) => CellState::Failed,
                None => CellState::Completed,
            },
        }
    }

    pub fn failure(&self) -> Option<&CellFailure> {
        match self {
            Self::Yielded { .. } => None,
            Self::Terminated { failure, .. } => Some(failure),
            Self::Finished { failure, .. } => failure.as_ref(),
        }
    }
}

/// Every way a session command can be refused. All of them are returned BEFORE
/// any output of another cell is read, so a bad identifier can never be used as
/// a way to observe someone else's cell.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum CodeModeError {
    #[error("unknown cell {cell_id}")]
    UnknownCell { cell_id: CellId },
    #[error("cell {cell_id} does not belong to session {session}")]
    ForeignCell { cell_id: CellId, session: SessionId },
    #[error("code mode session {session} is shut down")]
    SessionClosed { session: SessionId },
    #[error("session {session} already runs {active} cells (limit {limit})")]
    TooManyCells {
        session: SessionId,
        active: usize,
        limit: usize,
    },
    #[error("code mode engine unavailable: {detail}")]
    EngineUnavailable { detail: String },
}

/// Outcome of a session shutdown. `joined: false` is REPORTED rather than
/// swallowed: US-007 turns an unjoined worker into a visible failure instead of
/// silently detaching the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownReport {
    pub joined: bool,
    /// Cells whose terminal state had to be forced because the engine never
    /// confirmed it.
    pub forced_cells: Vec<CellId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ShutdownReport {
    pub fn joined() -> Self {
        Self {
            joined: true,
            forced_cells: Vec::new(),
            detail: None,
        }
    }

    pub fn unjoined(detail: impl Into<String>) -> Self {
        Self {
            joined: false,
            forced_cells: Vec::new(),
            detail: Some(detail.into()),
        }
    }
}
