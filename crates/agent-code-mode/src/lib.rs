//! Code Mode session protocol (EP-002, US-006).
//!
//! A Code Mode session belongs to exactly ONE thread and owns its cells. This
//! crate holds the contract and the state machine; it holds no JavaScript
//! engine, which is what makes `exec`, `wait`, `terminate` and `shutdown`
//! provable without one. `agent-code-mode-v8` implements `CellEngine` on a real
//! isolate, and `agent-tools` projects the pair onto the model-visible `exec`
//! and `wait` tools.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod nested;
pub mod protocol;
pub mod session;
pub mod tools;

pub use nested::{
    NestedToolBinding, NestedToolCall, NestedToolDispatcher, NestedToolInput, NestedToolOutcome,
    NoNestedTools, PlanDispatcher,
};
pub use protocol::{
    CellFailure, CellFailureKind, CellId, CellState, CodeModeError, DEFAULT_TERMINATE_GRACE,
    DEFAULT_YIELD_TIME, ExecuteRequest, ImageDetail, MAX_ACTIVE_CELLS, MAX_CELL_OUTPUT_BYTES,
    NestedTool, OutputItem, RuntimeResponse, SessionId, ShutdownReport, WaitRequest,
    normalize_binding,
};
pub use session::{CellEngine, CellSink, CodeModeSession, SessionLimits};
pub use tools::{
    EXEC_GRAMMAR, EXEC_PRAGMA_PREFIX, EXEC_TOOL_NAME, ExecInputError, ExecPragma, WAIT_TOOL_NAME,
    exec_tool_spec, parse_exec_source, wait_tool_spec,
};
