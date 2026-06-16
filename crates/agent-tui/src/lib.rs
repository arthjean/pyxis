//! `agent-tui` — frontend terminal de Numen (US-019). CLIENT du cœur headless :
//! il consomme les `agent_core::AgentEvent` (jamais d'ANSI venant du cœur) et
//! décide seul du rendu. Esthétique monochrome + un accent, épurée (Rauch/Vercel)
//! — la signature est une gouttière `▌` qui s'allume sur le tour en cours.
//!
//! Découpage : `state` (transcript + clavier, pur, testable), `render` (Ratatui
//! pur, `TestBackend`), `markdown` (réponses markdown → spans), `term` (raw mode,
//! alt screen). La boucle d'orchestration (crossterm ↔ stream d'agent ↔
//! permissions) vit dans `agent-cli`, qui assemble ces briques.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod markdown;
pub mod render;
pub mod state;
pub mod term;

pub use render::{render, Theme};
pub use state::{
    blocks_from_messages, prompts_from_messages, AppState, Block, DiffKind, DiffLine, InputAction,
    McpServerMeta, McpStatus, MenuItem, PermissionPrompt, SessionMeta, Status, COMMANDS,
};
pub use term::{enter, leave, supports_truecolor, Tui};
