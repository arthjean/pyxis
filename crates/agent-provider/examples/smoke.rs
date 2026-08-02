//! LIVE smoke test of the ChatGPT subscription adapter (Responses API, SSE).
//!
//! `cargo run -p agent-provider --example smoke -- "your prompt" [model]`
//!
//! Reads back the credential from the keyring (written by `agent-auth --example login`),
//! opens a real stream against the ChatGPT backend, and prints the text token by
//! token (reasoning greyed out on stderr). This is the end-to-end
//! "it works with my subscription" check: there is no CLI yet (EP-005).
//!
//! Note: the `model` slug depends on what your subscription exposes on the Codex backend
//! (default `DEFAULT_MODEL`). On a `400 ... not supported`, pass the right id
//! as the 2nd arg (versioned slugs: `gpt-5.4`, `gpt-5.5`, ...).

use agent_auth::{Credential, ProviderId, store};
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
        Some(Credential::Oauth(o)) if o.provider == ProviderId::OpenAiChatGpt => o,
        Some(Credential::Oauth(o)) => {
            eprintln!(
                "Credential ChatGPT invalide dans le keyring ({:?}). Relance :\n  cargo run -p agent-auth --example login",
                o.provider
            );
            std::process::exit(1);
        }
        _ => {
            eprintln!(
                "Pas de credential ChatGPT. Lance d'abord :\n  cargo run -p agent-auth --example login"
            );
            std::process::exit(1);
        }
    };

    let provider = OpenAiChatGptProvider::from_credential(cred)?;
    let req = CanonicalRequest {
        model,
        model_runtime: None,
        reasoning_effort: None,
        reasoning_replay: false,
        system: Some("Tu es Pyxis, un agent de codage concis.".to_string()),
        messages: vec![Message::user(prompt)],
        tools: vec![],
        max_output_tokens: 1024,
        ..CanonicalRequest::default()
    };

    let mut stream = provider.stream(req).await?;
    while let Some(ev) = stream.next().await {
        match ev? {
            StreamEvent::TextDelta { text } => {
                print!("{text}");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            // reasoning greyed out on stderr (does not clutter the output).
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
