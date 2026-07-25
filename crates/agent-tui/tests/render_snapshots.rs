//! Couverture snapshot des flux critiques du TUI (US-006,
//! `tasks/prd-harness-parity.md`).
//!
//! Chaque test fige une frame complète pour un flux nommé par le critère
//! d'acceptation. Le harness (`harness/mod.rs`) garantit le déterminisme, la
//! capture de panique et l'absence de débordement horizontal ; ces tests ne
//! s'occupent que de construire l'état.
//!
//! Réviser un diff : `cargo insta review` (ou `cargo insta test --review`).
//! Toute divergence de rendu assumée par rapport à Codex est enregistrée dans
//! `docs/codex-port-inventory.md`, section « Divergences de rendu assumées ».

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod harness;

use agent_core::AgentEvent;
use agent_core::error::AgentError;
use agent_core::event::{ToolCallView, ToolResultView};
use agent_core::message::{Message, ToolErrorKind};
use agent_tui::blocks_from_messages;
use agent_tui::state::{AppState, PermissionPrompt, SessionMeta};

const W: u16 = 80;
const H: u16 = 24;
/// Terminal étroit et terminal large (US-006 AC4).
const NARROW: u16 = 40;
const WIDE: u16 = 200;

/// État de base : modèle fixe, truecolor forcé, aucune lecture d'environnement.
fn state() -> AppState {
    let mut s = AppState::new("gpt-5", true);
    s.workspace = "pyxis".into();
    s
}

fn tool_call(id: &str, name: &str, input: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolCall(ToolCallView {
        id: id.to_string(),
        name: name.to_string(),
        input,
    })
}

fn tool_result(id: &str, content: &str, is_error: bool) -> AgentEvent {
    AgentEvent::ToolResult(ToolResultView {
        id: id.to_string(),
        content: content.to_string(),
        is_error,
        error_kind: is_error.then_some(ToolErrorKind::Semantic),
        untrusted: true,
    })
}

/// Transcript de référence réutilisé par les snapshots de géométrie.
fn conversation() -> AppState {
    let mut s = state();
    s.push_user("Explique la boucle d'agent et montre le code");
    s.apply(&AgentEvent::Text(
        "La boucle est un `Stream` d'`AgentEvent`.\n\nElle rend la main à chaque frontière.".into(),
    ));
    s.apply(&AgentEvent::EndTurn);
    s
}

// ───────────────────────────── Accueil ─────────────────────────────

#[test]
fn welcome_screen() {
    let s = state();
    insta::assert_snapshot!("welcome", harness::frame("welcome", &s, W, H));
}

#[test]
fn welcome_screen_narrow() {
    let s = state();
    insta::assert_snapshot!(
        "welcome_narrow",
        harness::frame("welcome_narrow", &s, NARROW, H)
    );
}

#[test]
fn welcome_screen_wide() {
    let s = state();
    insta::assert_snapshot!("welcome_wide", harness::frame("welcome_wide", &s, WIDE, H));
}

// ────────────────────────── Tour de conversation ──────────────────────────

#[test]
fn user_message() {
    let mut s = state();
    s.push_user("Relis `crates/agent-core/src/agent.rs` et dis-moi ce qui cloche");
    insta::assert_snapshot!("user_message", harness::frame("user_message", &s, W, H));
}

#[test]
fn streaming_deltas() {
    let mut s = state();
    s.push_user("Résume l'architecture");
    for delta in [
        "Le cœur ",
        "est ",
        "headless : ",
        "aucun ANSI ",
        "n'en sort.",
    ] {
        s.apply(&AgentEvent::Text(delta.into()));
    }
    s.spinner_tick = 3;
    s.tick_progress(std::time::Duration::from_secs(4));
    insta::assert_snapshot!(
        "streaming_deltas",
        harness::frame("streaming_deltas", &s, W, H)
    );
}

#[test]
fn reasoning_stream() {
    let mut s = state();
    s.push_user("Pourquoi ce test échoue ?");
    s.apply(&AgentEvent::Reasoning(
        "Le curseur de session n'est pas resynchronisé.\nDonc le delta est réécrit.".into(),
    ));
    s.spinner_tick = 1;
    s.tick_progress(std::time::Duration::from_secs(2));
    insta::assert_snapshot!(
        "reasoning_stream",
        harness::frame("reasoning_stream", &s, W, H)
    );
}

#[test]
fn markdown_code_block() {
    let mut s = state();
    s.push_user("Montre-moi le token d'annulation");
    s.apply(&AgentEvent::Text(
        "Voici la primitive :\n\n```rust\npub fn cancel(&self) {\n    self.tx.send_replace(true);\n}\n```\n\nElle est idempotente.".into(),
    ));
    s.apply(&AgentEvent::EndTurn);
    insta::assert_snapshot!(
        "markdown_code_block",
        harness::frame("markdown_code_block", &s, W, H)
    );
}

#[test]
fn markdown_table() {
    let mut s = state();
    s.push_user("Compare les modes de permission");
    s.apply(&AgentEvent::Text(
        "| Mode | Édition | Bash |\n|---|---|---|\n| Ask | demande | demande |\n| DontAsk | auto | demande |\n".into(),
    ));
    s.apply(&AgentEvent::EndTurn);
    insta::assert_snapshot!("markdown_table", harness::frame("markdown_table", &s, W, H));
}

// ─────────────────────────── Exécution d'outils ───────────────────────────

#[test]
fn exec_running() {
    let mut s = state();
    s.push_user("Compile le workspace");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo build --workspace" }),
    ));
    s.spinner_tick = 2;
    s.tick_progress(std::time::Duration::from_secs(7));
    insta::assert_snapshot!("exec_running", harness::frame("exec_running", &s, W, H));
}

#[test]
fn exec_success() {
    let mut s = state();
    s.push_user("Compile le workspace");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo build --workspace" }),
    ));
    s.apply(&tool_result(
        "call_1",
        "   Compiling agent-core v0.0.0\n    Finished `dev` profile in 12.40s",
        false,
    ));
    s.apply(&AgentEvent::EndTurn);
    insta::assert_snapshot!("exec_success", harness::frame("exec_success", &s, W, H));
}

#[test]
fn exec_error() {
    let mut s = state();
    s.push_user("Compile le workspace");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo build --workspace" }),
    ));
    s.apply(&tool_result(
        "call_1",
        "error[E0308]: mismatched types\n  --> crates/agent-core/src/agent.rs:431:9\nerror: could not compile `agent-core`",
        true,
    ));
    s.apply(&AgentEvent::EndTurn);
    insta::assert_snapshot!("exec_error", harness::frame("exec_error", &s, W, H));
}

#[test]
fn edit_diff() {
    let mut s = state();
    s.push_user("Renomme le champ `tx` en `sender`");
    s.apply(&tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "crates/agent-core/src/cancel.rs",
            "old_string": "pub struct CancelToken {\n    tx: Arc<watch::Sender<bool>>,\n}",
            "new_string": "pub struct CancelToken {\n    sender: Arc<watch::Sender<bool>>,\n}",
        }),
    ));
    s.apply(&tool_result("call_1", "edited 1 occurrence", false));
    s.apply(&AgentEvent::EndTurn);
    insta::assert_snapshot!("edit_diff", harness::frame("edit_diff", &s, W, H));
}

// ─────────────────────── Dialogues, saisie, menus ───────────────────────

#[test]
fn approval_dialog() {
    let mut s = state();
    s.push_user("Supprime le dossier de build");
    let preview = agent_tui::diff::note(["rm -rf target/debug/incremental"]);
    let mut prompt = PermissionPrompt::new(
        "Bash",
        "destructive command outside the read-only set",
        preview,
    );
    prompt.call_id = Some("call_1".into());
    prompt.mode = Some("ask".into());
    s.pending = Some(prompt);
    insta::assert_snapshot!(
        "approval_dialog",
        harness::frame("approval_dialog", &s, W, H)
    );
}

#[test]
fn pending_input() {
    let mut s = conversation();
    s.input = "Ajoute un test qui couvre la reprise de session".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!("pending_input", harness::frame("pending_input", &s, W, H));
}

#[test]
fn command_menu() {
    let mut s = state();
    s.input = "/".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!("command_menu", harness::frame("command_menu", &s, W, H));
}

#[test]
fn command_menu_filtered() {
    let mut s = state();
    s.input = "/re".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!(
        "command_menu_filtered",
        harness::frame("command_menu_filtered", &s, W, H)
    );
}

#[test]
fn resume_menu() {
    let mut s = state();
    s.sessions = vec![
        SessionMeta {
            id: "20260724-101500".into(),
            label: "Audit de parité harness".into(),
            hint: "42 msgs".into(),
        },
        SessionMeta {
            id: "20260723-183000".into(),
            label: "Port du composer Codex".into(),
            hint: "17 msgs".into(),
        },
    ];
    s.input = "/resume ".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!("resume_menu", harness::frame("resume_menu", &s, W, H));
}

#[test]
fn resumed_session() {
    let mut s = state();
    // Session reprise : le transcript est reconstruit depuis les messages
    // persistés, pas depuis des événements live.
    let messages = vec![
        Message::user("Où en est la réconciliation du transcript ?"),
        Message::assistant_text("Les appels en vol reçoivent un résultat synthétique."),
        Message::user("Et à la reprise ?"),
    ];
    s.blocks = blocks_from_messages(&messages);
    s.load_history(agent_tui::prompts_from_messages(&messages));
    s.context_pct = Some(38);
    insta::assert_snapshot!(
        "resumed_session",
        harness::frame("resumed_session", &s, W, H)
    );
}

#[test]
fn context_indicator() {
    let mut s = conversation();
    s.context_pct = Some(84);
    s.reasoning_effort = Some("high".into());
    insta::assert_snapshot!(
        "context_indicator",
        harness::frame("context_indicator", &s, W, H)
    );
}

// ──────────────────────── Interruption et erreurs ────────────────────────

#[test]
fn interrupted_turn() {
    let mut s = state();
    s.push_user("Lance la suite complète");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo test --workspace" }),
    ));
    // US-002 : l'appel en vol reçoit son résultat synthétique AVANT l'événement
    // d'interruption émis par le cœur.
    s.apply(&tool_result("call_1", "interrupted by user", true));
    s.apply(&AgentEvent::Interrupted);
    insta::assert_snapshot!(
        "interrupted_turn",
        harness::frame("interrupted_turn", &s, W, H)
    );
}

#[test]
fn error_block() {
    let mut s = state();
    s.push_user("Continue");
    s.apply(&AgentEvent::Error(AgentError::Session(
        "io: read-only file system".into(),
    )));
    insta::assert_snapshot!("error_block", harness::frame("error_block", &s, W, H));
}

// ───────────────────────────── Géométrie ─────────────────────────────

#[test]
fn resize_narrow() {
    let mut s = conversation();
    s.input = "Un prompt qui dépasse largement la largeur du terminal étroit".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!(
        "resize_narrow",
        harness::frame("resize_narrow", &s, NARROW, H)
    );
}

#[test]
fn resize_wide() {
    let mut s = conversation();
    s.input = "Un prompt qui dépasse largement la largeur du terminal étroit".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!("resize_wide", harness::frame("resize_wide", &s, WIDE, H));
}

#[test]
fn resize_short_terminal() {
    let mut s = conversation();
    s.input = "Le terminal est plus court que la mise en page ne le demande".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!(
        "resize_short_terminal",
        harness::frame("resize_short_terminal", &s, W, 8)
    );
}

#[test]
fn scrolled_transcript() {
    let mut s = state();
    for turn in 0..6 {
        s.push_user(format!("Question numéro {turn}"));
        s.apply(&AgentEvent::Text(format!(
            "Réponse numéro {turn}, assez longue pour occuper la largeur du terminal."
        )));
        s.apply(&AgentEvent::EndTurn);
    }
    s.scroll = 4;
    s.unseen = 2;
    insta::assert_snapshot!(
        "scrolled_transcript",
        harness::frame("scrolled_transcript", &s, W, H)
    );
}

// ─────────────── Chemin de rendu réellement embarqué (parity) ───────────────

/// Le mode inline-scrollback rend le transcript depuis `ChatSurface` et non
/// depuis `AppState::blocks` : sans ces snapshots, le chemin qui ship reste
/// découvert.
#[cfg(feature = "codex_tui_parity")]
#[test]
fn parity_surface_conversation() {
    let mut s = state();
    let messages = vec![
        Message::user("Quelle est la frontière d'arrêt ?"),
        Message::assistant_text("La fin d'événement de stream et le retour de dispatch."),
    ];
    s.blocks = blocks_from_messages(&messages);
    let surface = agent_tui::ChatSurface::from_messages(&messages);
    insta::assert_snapshot!(
        "parity_surface_conversation",
        harness::frame_parity("parity_surface_conversation", &s, &surface, W, H)
    );
}

#[cfg(feature = "codex_tui_parity")]
#[test]
fn parity_surface_pending_input() {
    let mut s = state();
    let messages = vec![Message::user("Récapitule")];
    s.blocks = blocks_from_messages(&messages);
    s.blocks
        .push(agent_tui::Block::Notice("context compacted".into()));
    s.input = "/models ".into();
    s.cursor = s.input.len();
    let surface = agent_tui::ChatSurface::from_messages(&messages);
    insta::assert_snapshot!(
        "parity_surface_pending_input",
        harness::frame_parity("parity_surface_pending_input", &s, &surface, W, H)
    );
}
