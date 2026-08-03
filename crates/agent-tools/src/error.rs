//! Errors of the tool system. `ValidationError` is the failure of `validate_input`
//! (pre-execution); `ToolError` is the error surfaced to the agent (serialized into
//! `ModelToolResult { is_error: true }`). Neither panics: the pipeline is
//! fail-closed (ARCHITECTURE 4.1, invariant 4).

use agent_core::ToolErrorKind;

/// Input validation failure of a tool (pre-permission, pre-execution).
#[derive(Debug, Clone, thiserror::Error)]
#[error("validation: {0}")]
pub struct ValidationError(pub String);

impl ValidationError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Execution error of a tool. Converted into `ModelToolResult { is_error: true }`
/// then returned to the agent as a `tool_result` (the model sees the failure and can
/// react); never propagated as a panic.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The JSON input does not parse into the tool schema (fail-closed:
    /// we do not execute, US-010 AC3).
    #[error("invalid argument: {0}")]
    Parse(String),
    /// `validate_input` refused the input.
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// Path outside the workspace (application-level confinement; the kernel enforces it
    /// through Landlock in US-020).
    #[error("path outside workspace: {0}")]
    OutsideWorkspace(String),
    /// I/O error (file not found, OS permission, etc.).
    #[error("io: {0}")]
    Io(String),
    /// A terminal session is no longer writable. Kept distinct so the
    /// registry can publish closure as structured execution metadata.
    #[error("{0}")]
    SessionClosed(String),
    /// Input rejected for a domain reason (ambiguous anchor, binary file, ...).
    #[error("{0}")]
    Rejected(String),
    /// The tool exceeded its timeout (reported by the Registry, not by the tool).
    #[error("timeout exceeded")]
    Timeout,
}

impl ToolError {
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            ToolError::Parse(_) => ToolErrorKind::Parse,
            ToolError::Validation(_) => ToolErrorKind::Validation,
            ToolError::OutsideWorkspace(_) => ToolErrorKind::OutsideWorkspace,
            ToolError::Io(_) => ToolErrorKind::Io,
            ToolError::SessionClosed(_) => ToolErrorKind::Rejected,
            ToolError::Rejected(_) => ToolErrorKind::Rejected,
            ToolError::Timeout => ToolErrorKind::Timeout,
        }
    }
}
