//! `agent-tui` — frontend terminal de Pyxis (US-019). CLIENT du cœur headless :
//! il consomme les `agent_core::AgentEvent` (jamais d'ANSI venant du cœur) et
//! décide seul du rendu. Esthétique monochrome + un accent, épurée (Rauch/Vercel)
//! — la signature est une gouttière `▌` qui s'allume sur le tour en cours.
//!
//! Découpage : `state` (transcript + clavier, pur, testable), `theme` (palette
//! monochrome + accent, pure), `render` (Ratatui pur, `TestBackend`), `markdown`
//! (réponses markdown → spans), `tool` (view-models d'outils → labels/résumés),
//! `term` (raw mode, alt screen). La boucle d'orchestration (crossterm ↔ stream
//! d'agent ↔ permissions) vit dans `agent-cli`, qui assemble ces briques.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "codex_tui_parity")]
pub mod app_event;
#[cfg(feature = "codex_tui_parity")]
pub mod bottom_pane;
mod cache;
pub mod diff;
mod highlight;
#[cfg(feature = "codex_tui_parity")]
pub mod history_cell;
#[cfg(feature = "codex_tui_parity")]
pub mod insert_history;
mod markdown;
mod measure;
pub mod render;
mod spinner;
pub mod state;
#[cfg(feature = "codex_tui_parity")]
pub mod streaming;
pub mod term;
#[cfg(feature = "codex_tui_parity")]
pub mod terminal_hyperlinks;
#[cfg(feature = "codex_tui_parity")]
pub mod terminal_viewport;
pub mod theme;
mod tool;

#[cfg(feature = "codex_tui_parity")]
pub use app_event::{
    AppDispatchOutcome, AppEvent as TuiAppEvent, AppEventDispatcher, PermissionTranscriptRequest,
    TranscriptExecSource, TranscriptExecStream, TranscriptHookOutputEntry,
    TranscriptHookOutputKind, TranscriptHookStatus, TranscriptItem, TranscriptItemId,
    TranscriptItemKind, TranscriptItemStatus, TranscriptLifecycle, TranscriptMapper,
    TranscriptNoticeKind, TranscriptNoticeLink, TranscriptPatchChangeKind,
    TranscriptPatchFileChange, TranscriptPayload, TranscriptPlanStep, TranscriptPlanStepStatus,
    TranscriptRole, TranscriptToolErrorKind, TranscriptUpdate, TranscriptUserInputAnswer,
    TranscriptUserInputQuestion,
};
#[cfg(feature = "codex_tui_parity")]
pub use bottom_pane::{
    BottomPane, BottomPaneView, ListSelectionView, SelectionRow, SelectionTab, ViewCompletion,
};
#[cfg(feature = "codex_tui_parity")]
pub use history_cell::{
    ActiveHistoryCell, AgentMarkdownCell, ApprovalCell, ChatSurface, CompositeCell, ErrorCell,
    ExecCell, FileChangeCell, FinalMessageSeparatorCell, HistoryCell, HistoryCellKind, HookCell,
    HookOutputEntry, HookOutputKind, HookStatus, McpInvocation, McpToolCell, NoticeCell,
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
pub use render::render;
#[cfg(feature = "codex_tui_parity")]
pub use render::render_parity;
pub use state::{
    AppState, Block, COMMANDS, DEFAULT_PERMISSION_MODE_ID, InputAction, McpServerMeta, McpStatus,
    MenuItem, PERMISSION_MODES, PermissionModeMeta, PermissionPrompt, SessionMeta, Status,
    blocks_from_messages, permission_mode_label, permission_mode_meta, prompts_from_messages,
};
#[cfg(feature = "codex_tui_parity")]
pub use streaming::{StreamController, StreamView};
pub use term::{Tui, clear, enter, leave, supports_truecolor};
#[cfg(feature = "codex_tui_parity")]
pub use terminal_viewport::{TerminalViewport, TerminalViewportState};
pub use theme::Theme;
