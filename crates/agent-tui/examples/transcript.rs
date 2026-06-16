//! Aperçu du rendu de réponse refondu (markdown, reasoning replié, tools
//! compacts) : `cargo run -p agent-tui --example transcript`. Rend une session
//! représentative dans un `TestBackend`, sans terminal réel.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::event::{ToolCallView, ToolResultView};
use agent_core::AgentEvent;
use agent_tui::{render, AppState};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn dump(state: &AppState, w: u16, h: u16, label: &str) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!(
        "\n── {label} {}",
        "─".repeat((w as usize).saturating_sub(label.len() + 4))
    );
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

fn tool(s: &mut AppState, name: &str, input: serde_json::Value, out: &str) {
    s.apply(&AgentEvent::ToolCall(ToolCallView {
        id: name.into(),
        name: name.into(),
        input,
    }));
    s.apply(&AgentEvent::ToolResult(ToolResultView {
        id: name.into(),
        content: out.into(),
        is_error: false,
        untrusted: true,
    }));
}

fn main() {
    let mut s = AppState::new("gpt-5.4", true);
    s.workspace = "numen".into();
    s.context_pct = Some(31);

    s.push_user("Explique le projet stp.");
    s.apply(&AgentEvent::Reasoning(
        "**Inspecting project files**\n\nI need to read the manifests and docs to \
         explain this well. Let me start with the Cargo workspace and the README."
            .into(),
    ));
    tool(
        &mut s,
        "glob",
        serde_json::json!({ "pattern": "**/*.toml" }),
        "Cargo.toml\ncrates/...",
    );
    tool(
        &mut s,
        "read",
        serde_json::json!({ "path": "README.md" }),
        "# Numen\n...",
    );
    tool(
        &mut s,
        "read",
        serde_json::json!({ "path": "Cargo.toml" }),
        "[workspace]\n...",
    );

    let answer = "**Numen** est un agent de code en terminal — la qualité de Claude Code, \
        ouvert aux modèles *frontier*.\n\n\
        ## Le produit\n\n\
        Une vraie TUI native (Rust + `ratatui`), pas un wrapper. L'objectif : orchestrer \
        un agent de codage dans ton workspace, avec les outils `read`, `glob`, `grep`, \
        `bash`.\n\n\
        Points clés :\n\
        - **Abonnement ChatGPT** comme provider (backend Codex, slug `gpt-5.4`).\n\
        - **Sandbox FS** via Landlock + proxy réseau allow-list.\n\
        - Rendu **markdown** propre, gouttière qui s'allume au stream.\n\n\
        ## Résumé\n\n\
        Numen = un daimon de code, *local-first*, qui parle tes modèles GPT.";
    for chunk in answer.split_inclusive(' ') {
        s.apply(&AgentEvent::Text(chunk.into()));
    }
    s.apply(&AgentEvent::EndTurn);

    dump(&s, 88, 30, "session complète (réponse markdown rendue)");

    // Scène 2 : réflexion EN COURS → repli + aperçu des dernières lignes pensées.
    let mut t = AppState::new("gpt-5.4", true);
    t.workspace = "numen".into();
    t.push_user("Refactore le parsing.");
    t.apply(&AgentEvent::Reasoning(
        "Let me think about the lexer.\n\nThe `todo!()` needs a real tokenizer. \
         I'll map chars to tokens and handle whitespace first."
            .into(),
    ));
    dump(&t, 88, 8, "réflexion en cours (aperçu replié)");
}
