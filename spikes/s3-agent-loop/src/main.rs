//! Live runner of the loop: Ollama (devstral, tool-capable) with a `bash` tool.
//! Best-effort: the local model may or may not emit the tool call; the rigorous
//! proof of the state machine lives in the tests (`ScriptedProvider`).
//!
//! Env: OLLAMA_BASE, OLLAMA_MODEL (default devstral-small-2:24b).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use spike_loop::{EndState, LiveProvider, run_agent};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let base = std::env::var("OLLAMA_BASE").unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "devstral-small-2:24b".into());

    let tools = serde_json::json!([{
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Exécute une commande shell et renvoie stdout/stderr.",
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string", "description": "la commande" } },
                "required": ["cmd"],
            },
        },
    }]);

    let provider = LiveProvider {
        base: base.clone(),
        api_key: None,
        model: model.clone(),
        tools: Some(tools),
    };

    let user = std::env::args().nth(1).unwrap_or_else(|| {
        "Utilise l'outil bash pour exécuter exactement `echo bonjour depuis pyxis`, \
         puis dis-moi la sortie."
            .to_string()
    });

    println!("[s3] base={base} model={model}");
    println!("[s3] prompt={user:?}\n");

    let out = run_agent(
        &provider,
        Some("Tu es un agent de codage. Quand une commande shell est demandée, utilise l'outil bash."),
        &user,
        6,
        Duration::from_secs(20),
    )
    .await;

    println!("\n──────── trace ────────");
    println!("tours: {}", out.turns);
    for (i, inv) in out.invocations.iter().enumerate() {
        println!(
            "outil #{i}: {} args={:?} timeout={} untrusted={}\n  -> {:?}",
            inv.name, inv.args, inv.timed_out, inv.untrusted, inv.output
        );
    }
    println!("texte final: {:?}", out.final_text);
    match &out.ended {
        EndState::EndTurn => println!("[s3] fin: EndTurn propre ✓"),
        EndState::Exhausted(w) => println!("[s3] fin: Exhausted({w})"),
        EndState::Fail(e) => println!("[s3] fin: Fail({e})"),
    }

    if out.invocations.is_empty() {
        println!(
            "[s3] NB: le modèle local n'a pas appelé l'outil ce coup-ci (non déterministe). \
             La preuve de la boucle est dans `cargo test -p s3-agent-loop`."
        );
    }
    Ok(())
}
