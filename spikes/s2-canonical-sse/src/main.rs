//! Live runner of the canonical layer: streams an OpenAI-compatible provider and
//! prints every decoded `StreamEvent`. By default: local Ollama (no secret).
//!
//! Usage:
//!   s2-canonical-sse ["prompt"]
//! Env:
//!   OLLAMA_BASE   (default http://localhost:11434/v1)
//!   OLLAMA_MODEL  (default devstral-small-2:24b)
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use futures_util::StreamExt;
use spike_canon::{StreamEvent, build_body, stream_chat};

#[tokio::main]
async fn main() -> Result<()> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Dis bonjour en exactement cinq mots.".to_string());
    let base = std::env::var("OLLAMA_BASE").unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "devstral-small-2:24b".into());

    println!("[s2] base={base} model={model}");
    println!("[s2] prompt={prompt:?}\n");

    let body = build_body(
        &model,
        serde_json::json!([{ "role": "user", "content": prompt }]),
        None,
    );

    let mut stream = match stream_chat(&base, None, body).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[s2] ÉCHEC ouverture stream: {e}");
            eprintln!("[s2] (Ollama démarré ? `ollama serve` + modèle pull ?)");
            return Err(e.into());
        }
    };

    let mut text = String::new();
    let mut n_events = 0usize;
    while let Some(ev) = stream.next().await {
        n_events += 1;
        match ev {
            Ok(StreamEvent::TextDelta { text: t }) => {
                print!("{t}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                text.push_str(&t);
            }
            Ok(StreamEvent::ReasoningDelta { text: t }) => eprint!("\x1b[2m{t}\x1b[0m"),
            Ok(StreamEvent::ToolCallStart { id, name }) => {
                println!("\n[event] ToolCallStart id={id} name={name}")
            }
            Ok(StreamEvent::ToolCallDelta { id, args_json }) => {
                println!("[event] ToolCallDelta id={id} args+={args_json:?}")
            }
            Ok(StreamEvent::ToolCallEnd { id }) => println!("[event] ToolCallEnd id={id}"),
            Ok(StreamEvent::Usage { usage }) => println!("\n[event] Usage {usage:?}"),
            Ok(StreamEvent::Done { stop }) => println!("\n[event] Done stop={stop:?}"),
            Err(e) => {
                eprintln!("\n[s2] erreur stream classifiée (pas de panic): {e}");
                break;
            }
        }
    }

    println!(
        "\n[s2] {n_events} StreamEvent décodés. texte={} chars",
        text.len()
    );
    Ok(())
}
