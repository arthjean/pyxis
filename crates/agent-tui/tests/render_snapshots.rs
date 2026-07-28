//! Snapshot coverage of the critical TUI flows (US-006,
//! `tasks/prd-harness-parity.md`).
//!
//! Each test freezes a full frame for a flow named after its acceptance
//! criterion. The harness (`harness/mod.rs`) guarantees determinism, panic
//! capture and the absence of horizontal overflow; these tests only
//! take care of building the state.
//!
//! Reviewing a diff: `cargo insta review` (or `cargo insta test --review`).
//! Every rendering divergence from Codex that we accept is recorded in
//! `docs/codex-port-inventory.md`, section "Divergences de rendu assumées".

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod harness;

use agent_core::AgentEvent;
use agent_core::error::AgentError;
use agent_core::event::{ToolCallView, ToolOutputDeltaView, ToolResultView};
use agent_core::message::{Message, ToolErrorKind};
use agent_tui::blocks_from_messages;
use agent_tui::state::{AppState, PermissionPrompt, SessionMeta};

const W: u16 = 80;
const H: u16 = 24;
/// Narrow terminal and wide terminal (US-006 AC4).
const NARROW: u16 = 40;
const WIDE: u16 = 200;

/// Base state: fixed model, truecolor forced, no environment read.
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

/// Reference transcript reused by the geometry snapshots.
fn conversation() -> AppState {
    let mut s = state();
    s.push_user("Explique la boucle d'agent et montre le code");
    s.apply(&AgentEvent::Text(
        "La boucle est un `Stream` d'`AgentEvent`.\n\nElle rend la main à chaque frontière.".into(),
    ));
    s.apply(&AgentEvent::EndTurn);
    s
}

// ───────────────────────────── Welcome ─────────────────────────────

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

// ────────────────────────── Conversation turn ──────────────────────────

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

// ─────────────────────────── Tool execution ───────────────────────────

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
fn exec_streaming_output() {
    // US-015 AC2: the output arrives in the running execution cell, before
    // any result, and stays bounded to the last lines.
    let mut s = state();
    s.push_user("Compile le workspace");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo build --workspace" }),
    ));
    for krate in [
        "agent-core",
        "agent-session",
        "agent-tools",
        "agent-provider",
    ] {
        s.apply(&AgentEvent::ToolOutputDelta(ToolOutputDeltaView {
            id: "call_1".to_string(),
            chunk: format!("   Compiling {krate} v0.0.0\n"),
        }));
    }
    s.spinner_tick = 2;
    s.tick_progress(std::time::Duration::from_secs(7));
    insta::assert_snapshot!(
        "exec_streaming_output",
        harness::frame("exec_streaming_output", &s, W, H)
    );
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

// ─────────────────────── Dialogs, input, menus ───────────────────────

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

/// US-009 AC1: a memoizable request offers the session scopes on top of the
/// one-shot answers.
#[test]
fn approval_dialog_memoizable() {
    let mut s = state();
    s.push_user("Montre l'état du dépôt");
    let preview = agent_tui::diff::note(["git status"]);
    let mut prompt =
        PermissionPrompt::new("bash", "sensitive action requires confirmation", preview);
    prompt.call_id = Some("call_2".into());
    prompt.mode = Some("ask".into());
    prompt.memoizable = true;
    s.pending = Some(prompt);
    insta::assert_snapshot!(
        "approval_dialog_memoizable",
        harness::frame("approval_dialog_memoizable", &s, W, H)
    );
}

/// US-009 AC2/AC4: no session option when the command is not rememberable, the
/// reason visible, and everything readable on 40 columns.
#[test]
fn approval_dialog_narrow_not_memoizable() {
    let mut s = state();
    let preview = agent_tui::diff::note(["rm $(cat liste)"]);
    let mut prompt =
        PermissionPrompt::new("bash", "sensitive action requires confirmation", preview);
    prompt.call_id = Some("call_3".into());
    prompt.mode = Some("ask".into());
    prompt.memo_note = Some("the command contains a substitution or a variable".into());
    s.pending = Some(prompt);
    insta::assert_snapshot!(
        "approval_dialog_narrow_not_memoizable",
        harness::frame("approval_dialog_narrow_not_memoizable", &s, NARROW, H)
    );
}

/// US-009 AC4: the four options stack instead of being clipped when the
/// terminal is too narrow for a single row.
#[test]
fn approval_dialog_narrow_memoizable() {
    let mut s = state();
    let preview = agent_tui::diff::note(["git status"]);
    let mut prompt =
        PermissionPrompt::new("bash", "sensitive action requires confirmation", preview);
    prompt.call_id = Some("call_4".into());
    prompt.mode = Some("ask".into());
    prompt.memoizable = true;
    s.pending = Some(prompt);
    insta::assert_snapshot!(
        "approval_dialog_narrow_memoizable",
        harness::frame("approval_dialog_narrow_memoizable", &s, NARROW, H)
    );
}

#[test]
fn pending_input() {
    let mut s = conversation();
    s.input = "Ajoute un test qui couvre la reprise de session".into();
    s.cursor = s.input.len();
    insta::assert_snapshot!("pending_input", harness::frame("pending_input", &s, W, H));
}

/// Ten-line input: the composer height follows the number of rendered
/// lines, up to the cap (US-010 AC2).
#[test]
fn composer_multiline() {
    let mut s = conversation();
    s.set_input(
        (1..=10)
            .map(|i| format!("ligne {i} du prompt"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    insta::assert_snapshot!(
        "composer_multiline",
        harness::frame("composer_multiline", &s, W, H)
    );
}

/// Line wider than the terminal: folded over several visual lines,
/// no character lost, no horizontal overflow (US-010 AC1).
#[test]
fn composer_wrapped_line() {
    let mut s = conversation();
    s.set_input(
        "Analyse la boucle d'agent, la frontiere d'annulation cooperative et la \
         reconciliation du transcript, puis propose un plan de test."
            .into(),
    );
    insta::assert_snapshot!(
        "composer_wrapped_line",
        harness::frame("composer_wrapped_line", &s, NARROW, H)
    );
}

/// Input past the cap: the area scrolls to keep the cursor line
/// visible, the transcript keeps the rest of the screen (US-010 AC3).
#[test]
fn composer_scrolled() {
    let mut s = conversation();
    s.set_input(
        (1..=20)
            .map(|i| format!("paragraphe {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    insta::assert_snapshot!(
        "composer_scrolled",
        harness::frame("composer_scrolled", &s, W, H)
    );
}

/// Terminal shorter than the height requested by the composer: transcript and
/// composer both stay visible (US-010 AC6).
#[test]
fn composer_short_terminal() {
    let mut s = conversation();
    s.set_input(
        (1..=12)
            .map(|i| format!("ligne {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    insta::assert_snapshot!(
        "composer_short_terminal",
        harness::frame("composer_short_terminal", &s, W, 8)
    );
}

/// Large paste: the composer shows a compact summary, not 847 lines
/// (US-011 AC2).
#[test]
fn composer_large_paste() {
    let mut s = conversation();
    let big = (0..847)
        .map(|i| format!("2026-07-25T10:00:{i:02} INFO ligne de log {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    s.insert_paste(&big);
    s.insert_str(" corrige la cause");
    insta::assert_snapshot!(
        "composer_large_paste",
        harness::frame("composer_large_paste", &s, W, H)
    );
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
    // Resumed session: the transcript is rebuilt from the persisted
    // messages, not from live events.
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

/// US-004 AC6: fed by real counters, the indicator must not push the status line
/// past a narrow terminal. The frame is captured at `NARROW` width, so any
/// overflow would show up as a truncated or wrapped line in the snapshot.
#[test]
fn context_indicator_narrow() {
    let mut s = conversation();
    s.apply(&AgentEvent::ModelTurn(agent_core::ModelTurnView {
        index: 1,
        input_tokens: 184_220,
        output_tokens: 12_900,
        context_tokens: Some(231_200),
        context_window: Some(272_000),
        auto_compact_token_limit: Some(244_800),
        estimated_context_tokens: None,
    }));
    s.reasoning_effort = Some("high".into());
    assert_eq!(
        s.context_pct,
        Some(85),
        "alimenté par les compteurs backend"
    );
    insta::assert_snapshot!(
        "context_indicator_narrow",
        harness::frame("context_indicator_narrow", &s, NARROW, H)
    );
}

// ──────────────────────── Interruption and errors ────────────────────────

#[test]
fn interrupted_turn() {
    let mut s = state();
    s.push_user("Lance la suite complète");
    s.apply(&tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "command": "cargo test --workspace" }),
    ));
    // US-002: the in-flight call gets its synthetic result BEFORE the
    // interruption event emitted by the core.
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

// ───────────────────────────── Geometry ─────────────────────────────

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

// ─────────────── Rendering path actually shipped (parity) ───────────────

/// The inline-scrollback mode renders the transcript from `ChatSurface` and not
/// from `AppState::blocks`: without these snapshots, the path that ships stays
/// uncovered.
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

// ───────────────────────── Runtime state (EP-005) ─────────────────────────

/// State of a thread the runtime is driving: identifiers, turn, state and the
/// inputs waiting behind it.
fn runtime_state() -> AppState {
    let mut s = conversation();
    s.thread_id = "th_9f2c4a17b3d84e0192cf5a7b6d3e8140".into();
    s.turn_id = Some("tu_5b1e07d4c2a9f38610be4d27a95c0f3e".into());
    s.turn_state = Some("running".into());
    s.pending_inputs = 2;
    s
}

/// US-017 AC9: the runtime status rendered on a 40-column terminal. The harness
/// itself fails the test on any horizontal overflow, so the snapshot is the
/// record of what the user reads and the check is mechanical.
#[test]
fn runtime_status_narrow() {
    let mut s = runtime_state();
    s.blocks
        .push(agent_tui::Block::Notice(agent_tui::session_status_report(
            &s,
            agent_tui::SessionFacts {
                session_id: "20260728-101112.jsonl",
                sandbox: "enforced (workspace)",
                config_sources: &[],
                profile: None,
                runtime: agent_tui::RuntimeFacts {
                    active_agents: 0,
                    max_active_agents: 4,
                    max_agents_per_root: 8,
                    max_agent_depth: 1,
                    command_mailbox: 64,
                    max_pending_inputs: 16,
                },
            },
        )));
    insta::assert_snapshot!(
        "runtime_status_narrow",
        harness::frame("runtime_status_narrow", &s, NARROW, 32)
    );
}

/// US-017 AC5: a steer accepted while a turn runs is announced, and the pending
/// count the runtime reports is what the state carries.
#[test]
fn runtime_steering_notice() {
    let mut s = runtime_state();
    s.blocks.push(agent_tui::Block::Notice(
        "Steering the current turn.".into(),
    ));
    insta::assert_snapshot!(
        "runtime_steering_notice",
        harness::frame("runtime_steering_notice", &s, W, H)
    );
}
