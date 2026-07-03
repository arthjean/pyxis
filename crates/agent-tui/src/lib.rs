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
pub mod terminal_viewport;
pub mod theme;
mod tool;

#[cfg(feature = "codex_tui_parity")]
pub use app_event::{
    AppDispatchOutcome, AppEvent as TuiAppEvent, AppEventDispatcher, TranscriptExecSource,
    TranscriptExecStream, TranscriptItem, TranscriptItemId, TranscriptItemKind,
    TranscriptItemStatus, TranscriptLifecycle, TranscriptMapper, TranscriptPayload, TranscriptRole,
    TranscriptToolErrorKind, TranscriptUpdate,
};
#[cfg(feature = "codex_tui_parity")]
pub use bottom_pane::{
    BottomPane, BottomPaneView, ListSelectionView, SelectionRow, SelectionTab, ViewCompletion,
};
#[cfg(feature = "codex_tui_parity")]
pub use history_cell::{
    ActiveHistoryCell, AgentMarkdownCell, ChatSurface, CompositeCell, ErrorCell, ExecCell,
    FileChangeCell, HistoryCell, HistoryCellKind, NoticeCell, ReasoningCell, ToolCell, UserCell,
    cells_from_messages,
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
    AppState, Block, COMMANDS, InputAction, McpServerMeta, McpStatus, MenuItem, PermissionPrompt,
    SessionMeta, Status, blocks_from_messages, prompts_from_messages,
};
#[cfg(feature = "codex_tui_parity")]
pub use streaming::{StreamController, StreamView};
pub use term::{Tui, enter, leave, supports_truecolor};
#[cfg(feature = "codex_tui_parity")]
pub use terminal_viewport::{TerminalViewport, TerminalViewportState};
pub use theme::Theme;
