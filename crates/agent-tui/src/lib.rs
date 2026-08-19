//! `agent-tui`: terminal frontend of Pyxis (US-019). CLIENT of the headless core:
//! it consumes the `agent_core::AgentEvent` (never ANSI coming from the core) and
//! alone decides the rendering. Monochrome aesthetics + one accent, spare (Rauch/Vercel):
//! the signature is a `▌` gutter that lights up on the turn in progress.
//!
//! Split: `state` (transcript + keyboard, pure, testable), `theme` (monochrome
//! palette + accent, pure), `render` (pure Ratatui, `TestBackend`), `markdown`
//! (markdown replies -> spans), `tool` (tool view-models -> labels/summaries),
//! `term` (raw mode, alt screen). The orchestration loop (crossterm <-> agent
//! stream <-> permissions) lives in `agent-cli`, which assembles these bricks.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "codex_tui_parity")]
pub mod app_event;
#[cfg(feature = "codex_tui_parity")]
pub mod bottom_pane;
mod cache;
#[cfg(feature = "codex_tui_parity")]
pub mod chatwidget;
mod composer;
pub mod custom_terminal;
pub mod debug_log;
pub mod diff;
pub mod footer;
mod highlight;
#[cfg(feature = "codex_tui_parity")]
pub mod history_cell;
#[cfg(feature = "codex_tui_parity")]
pub mod insert_history;
mod markdown;
mod measure;
#[cfg(feature = "codex_tui_parity")]
pub mod parse_command;
pub mod render;
mod spinner;
pub mod state;
#[cfg(feature = "codex_tui_parity")]
pub mod streaming;
pub mod term;
#[cfg(feature = "codex_tui_parity")]
pub mod terminal_hyperlinks;
pub mod theme;
mod tool;

#[cfg(feature = "codex_tui_parity")]
pub use app_event::{
    PermissionTranscriptRequest, TranscriptExecSource, TranscriptExecStream,
    TranscriptHookOutputEntry, TranscriptHookOutputKind, TranscriptHookStatus, TranscriptItem,
    TranscriptItemId, TranscriptItemKind, TranscriptItemStatus, TranscriptLifecycle,
    TranscriptMapper, TranscriptNoticeKind, TranscriptNoticeLink, TranscriptPatchChangeKind,
    TranscriptPatchFileChange, TranscriptPayload, TranscriptPlanStep, TranscriptPlanStepStatus,
    TranscriptRole, TranscriptToolErrorKind, TranscriptUpdate, TranscriptUserInputAnswer,
    TranscriptUserInputQuestion,
};
#[cfg(feature = "codex_tui_parity")]
pub use bottom_pane::{
    BottomPane, BottomPaneView, ListSelectionView, SelectionRow, SelectionTab, ViewCompletion,
};
#[cfg(feature = "codex_tui_parity")]
pub use chatwidget::ChatWidget;
pub use custom_terminal::Frame;
pub use footer::{FooterMode, FooterProps, StatusAccent, StatusSegment};
#[cfg(feature = "codex_tui_parity")]
pub use history_cell::{
    ActiveHistoryCell, ActivityHeader, AgentMarkdownCell, ApprovalCell, ChatSurface, CompositeCell,
    ErrorCell, ExecCell, FileChangeCell, FinalMessageSeparatorCell, HistoryCell, HistoryCellKind,
    HookCell, HookOutputEntry, HookOutputKind, HookStatus, McpInvocation, McpToolCell, NoticeCell,
    PatchApplyFailureCell, PatchChangeKind, PatchFileChange, PatchSummaryCell, PlanStep,
    PlanStepStatus, PlanUpdateCell, ReasoningCell, RequestUserInputCell, SessionHeaderCell,
    SpecialNoticeCell, SpecialNoticeKind, SpecialNoticeLink, ToolCell, UserCell, UserInputAnswer,
    UserInputQuestion, WebSearchCell, cells_from_messages,
};
#[cfg(feature = "codex_tui_parity")]
pub use insert_history::{
    HistoryInsertError, HistoryInserter, InsertHistoryMode, PendingHistoryInsert,
    SanitizedHistoryLine,
};
#[cfg(feature = "codex_tui_parity")]
pub use render::parity_content_height;
pub use render::render;
pub use state::{
    AppState, Block, COMMANDS, DEFAULT_PERMISSION_MODE_ID, InputAction, McpServerMeta, McpStatus,
    MenuItem, ModelCatalogEntry, ModelMeta, PERMISSION_MODES, PermissionModeMeta, PermissionPrompt,
    REASONING_EFFORTS, ReasoningEffortMeta, RuntimeFacts, SOURCE_KEY_MODEL,
    SOURCE_KEY_PERMISSION_MODE, SOURCE_KEY_REASONING_EFFORT, SOURCE_KEY_SANDBOX_MODE, SessionFacts,
    SessionMeta, Status, blocks_from_messages, default_reasoning_effort_for_model, model_meta,
    models, normalize_reasoning_effort, normalize_reasoning_effort_for_model,
    permission_mode_label, permission_mode_meta, plan_status_glyph, plan_status_label,
    prompts_from_messages, reasoning_effort_label, session_status_report, session_usage_report,
    set_models, supported_reasoning_efforts_for_model, turn_diff_summary,
};
#[cfg(feature = "codex_tui_parity")]
pub use streaming::{
    COMMIT_TICK_INTERVAL, ChunkingMode, ChunkingPolicy, StreamController, StreamView,
};
pub use term::{
    REFLOW_DEBOUNCE, Tui, clear, clear_for_reflow, clear_including_scrollback, draw, enter,
    is_active, leave, restore, supports_truecolor,
};
pub use theme::Theme;
