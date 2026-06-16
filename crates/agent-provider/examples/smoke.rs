//! Smoke test LIVE de l'adapter abonnement ChatGPT (Responses API, SSE).
//!
//! `cargo run -p agent-provider --example smoke -- "ton prompt" [model]`
//!
//! Relit la credential du keyring (écrite par `agent-auth --example login`),
//! ouvre un flux réel contre le backend ChatGPT, et imprime le texte token par
//! token (raisonnement en grisé sur stderr). C'est la vérification de bout en
//! bout « ça marche avec mon abonnement » — il n'y a pas encore de CLI (EP-005).
//!
//! ⚠️ Le slug `model` dépend de ce que ton abonnement expose côté backend Codex
//! (défaut `DEFAULT_MODEL`). En cas de `400 ... not supported`, passe le bon id
//! en 2e arg (slugs versionnés : `gpt-5.4`, `gpt-5.5`…).

use agent_auth::{Credential, store};
use agent_core::message::Message;
use agent_core::provider::{CanonicalRequest, Provider, StreamEvent};
use agent_provider::{DEFAULT_MODEL, KEYRING_ACCOUNT, OpenAiChatGptProvider};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let prompt = args
        .next()
        .unwrap_or_else(|| "Dis bonjour en une phrase.".to_string());
    let model = args.next().unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let cred = match store::load(KEYRING_ACCOUNT)? {
        Some(Credential::Oauth(o)) => o,
        _ => {
            eprintln!(
                "Pas de credential ChatGPT. Lance d'abord :\n  cargo run -p agent-auth --example login"
            );
            std::process::exit(1);
        }
    };

    let provider = OpenAiChatGptProvider::from_credential(cred);
    let req = CanonicalRequest {
        model,
        system: Some("Tu es Numen, un agent de codage concis.".to_string()),
        messages: vec![Message::user(prompt)],
        tools: vec![],
        max_output_tokens: 1024,
    };

    let mut stream = provider.stream(req).await?;
    while let Some(ev) = stream.next().await {
        match ev? {
            StreamEvent::TextDelta { text } => {
                print!("{text}");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            // raisonnement en grisé sur stderr (n'encombre pas la sortie).
            StreamEvent::ReasoningDelta { text } => eprint!("\x1b[2m{text}\x1b[0m"),
            StreamEvent::ToolCallStart { name, .. } => eprintln!("\n[tool: {name}]"),
            StreamEvent::Usage { usage } => {
                eprintln!("\n[usage: {} in / {} out]", usage.input, usage.output)
            }
            StreamEvent::Done { stop } => println!("\n[fin: {stop:?}]"),
            _ => {}
        }
    }
    Ok(())
}
