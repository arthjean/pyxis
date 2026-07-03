//! Aperçu statique du rendu (sans terminal réel) : `cargo run -p agent-tui
//! --example gallery`. Dump une scène représentative dans un `TestBackend` pour
//! eyeball l'esthétique (gouttière live, tool calls, diff de permission).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentEvent;
use agent_core::event::{ToolCallView, ToolResultView};
use agent_tui::state::PermissionPrompt;
use agent_tui::{AppState, diff, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn dump(state: &AppState, w: u16, h: u16, label: &str) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!(
        "\n── {label} ({w}×{h}) {}",
        "─".repeat((w as usize).saturating_sub(label.len() + 12))
    );
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("│{}│", line.trim_end());
    }
}

fn main() {
    // Scène 1 : conversation en cours de stream.
    let mut s = AppState::new("gpt-5", true);
    s.apply(&AgentEvent::Text("".into()));
    s.blocks.clear();
    s.push_user("refactore la fonction de parsing dans src/lexer.rs");
    s.apply(&AgentEvent::Reasoning(
        "je dois d'abord lire le fichier".into(),
    ));
    s.apply(&AgentEvent::ToolCall(ToolCallView {
        id: "c1".into(),
        name: "read".into(),
        input: serde_json::json!({ "path": "src/lexer.rs" }),
    }));
    s.apply(&AgentEvent::ToolResult(ToolResultView {
        id: "c1".into(),
        content: "   1\tfn lex(input: &str) -> Vec<Token> {\n   2\t    todo!()\n   3\t}".into(),
        is_error: false,
        untrusted: true,
        error_kind: None,
    }));
    s.apply(&AgentEvent::Text("Je remplace le ".into()));
    s.apply(&AgentEvent::Text("`todo!()` par un vrai lexer.".into()));
    dump(&s, 64, 16, "session live");

    // Scène 2 : dialog de permission avec diff (edit).
    let mut p = AppState::new("gpt-5", true);
    p.push_user("corrige le bug");
    p.apply(&AgentEvent::Text("J'applique le correctif.".into()));
    p.pending = Some(PermissionPrompt {
        title: "edit src/lexer.rs".into(),
        reason: "action sensible nécessitant confirmation".into(),
        preview: diff::from_tool(
            "edit",
            &serde_json::json!({
                "path": "src/lexer.rs",
                "old_string": "fn lex(input: &str) -> Vec<Token> {\n    todo!()\n}",
                "new_string": "fn lex(input: &str) -> Vec<Token> {\n    input.chars().map(Token::from).collect()\n}"
            }),
        )
        .unwrap_or_default(),
    });
    dump(&p, 64, 14, "permission + diff");

    // Scène 3 : dégradation monochrome (sans truecolor).
    let mut m = AppState::new("gpt-5", false);
    m.push_user("liste les fichiers Rust");
    m.apply(&AgentEvent::ToolCall(ToolCallView {
        id: "c2".into(),
        name: "glob".into(),
        input: serde_json::json!({ "pattern": "**/*.rs" }),
    }));
    m.apply(&AgentEvent::Text("Voici les fichiers trouvés.".into()));
    m.apply(&AgentEvent::EndTurn);
    dump(&m, 48, 10, "monochrome (no truecolor)");
}
