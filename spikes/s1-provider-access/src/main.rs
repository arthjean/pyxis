//! US-001: provider access spike (auth go/no-go).
//!
//! Determines with which credentials the end user talks to the model WITHOUT
//! being blocked: the Phase 0 go/no-go (see docs/PROVIDERS.md 6, ADR-7 R1).
//!
//! Three legs:
//!   - `ollama`: local, no credential -> runnable proof right here.
//!   - `openai`: token API key (OPENAI_API_KEY) -> streaming + usage (cost).
//!   - `anthropic`: probes the blocking of third-party tools; captures the exact message.
//!
//! The OpenAI/Anthropic legs read their keys from the env: you run them, and the verdict
//! completes. The Ollama leg already settles the unblocked path of the MVP.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use futures_util::StreamExt;
use spike_canon::{AdapterError, StreamEvent, build_body, stream_chat};

#[tokio::main]
async fn main() -> Result<()> {
    let leg = std::env::args().nth(1).unwrap_or_default();
    match leg.as_str() {
        "ollama" => leg_ollama().await,
        "openai" => leg_openai().await,
        "anthropic" => leg_anthropic().await,
        other => {
            eprintln!("usage: s1-provider-access <ollama|openai|anthropic>  (reçu: {other:?})");
            Ok(())
        }
    }
}

/// Streams an OpenAI-compatible endpoint, aggregates text + usage. Returns (chars, usage?).
async fn run_openai_compat(
    base: &str,
    key: Option<&str>,
    model: &str,
    prompt: &str,
) -> Result<(usize, Option<spike_canon::TokenUsage>), AdapterError> {
    let body = build_body(
        model,
        serde_json::json!([{ "role": "user", "content": prompt }]),
        None,
    );
    let mut stream = stream_chat(base, key, body).await?;

    let mut chars = 0usize;
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev? {
            StreamEvent::TextDelta { text } => {
                print!("{text}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                chars += text.chars().count();
            }
            StreamEvent::Usage { usage: u } => usage = Some(u),
            StreamEvent::Done { stop } => {
                println!("\n[stop] {stop:?}");
            }
            _ => {}
        }
    }
    Ok((chars, usage))
}

async fn leg_ollama() -> Result<()> {
    let base = std::env::var("OLLAMA_BASE").unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "devstral-small-2:24b".into());
    println!("=== leg OLLAMA (local, sans credential) ===");
    println!("base={base} model={model}\n");

    match run_openai_compat(&base, None, &model, "Réponds par un seul mot : OK.").await {
        Ok((chars, usage)) => {
            println!("\n[ollama] stream reçu : {chars} chars, sans credential distante ✓");
            match usage {
                Some(u) => println!("[ollama] usage présent: {u:?}"),
                None => println!(
                    "[ollama] usage ABSENT du stream → fallback tokenizer requis (cf. US-007/US-016)"
                ),
            }
            println!("[ollama] VERDICT: provider local NON-BLOQUÉ, viable comme défaut MVP.");
            Ok(())
        }
        Err(e) => {
            eprintln!("[ollama] ÉCHEC: {e}");
            eprintln!("[ollama] Ollama démarré ? `ollama serve` puis `ollama pull <model>`.");
            Err(e.into())
        }
    }
}

async fn leg_openai() -> Result<()> {
    println!("=== leg OPENAI (API key au token) ===");
    let Ok(key) = std::env::var("OPENAI_API_KEY") else {
        println!("OPENAI_API_KEY absente. Pour compléter le verdict :");
        println!("  OPENAI_API_KEY=sk-... cargo run -p s1-provider-access -- openai");
        println!("[openai] leg NON exécuté (clé manquante) — neutre pour le go/no-go.");
        return Ok(());
    };
    let base = std::env::var("OPENAI_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    println!("base={base} model={model}\n");

    match run_openai_compat(&base, Some(&key), &model, "Réponds par un seul mot : OK.").await {
        Ok((chars, usage)) => {
            println!("\n[openai] stream reçu : {chars} chars ✓");
            match usage {
                Some(u) => println!(
                    "[openai] usage metered: {u:?} → coût calculable (prompt+completion) ✓"
                ),
                None => println!(
                    "[openai] usage absent — vérifier stream_options.include_usage (devrait être présent)"
                ),
            }
            println!("[openai] VERDICT: API key au token NON-BLOQUÉE (hors abonnement).");
            Ok(())
        }
        Err(AdapterError::Http { status, body }) => {
            println!(
                "\n[openai] HTTP {status} → classe {}",
                classify_http(status, &body)
            );
            println!("[openai] body: {body}");
            if status == 401 {
                println!(
                    "[openai] clé invalide/expirée (Auth) — message clair affiché (unhappy path AC2)."
                );
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("[openai] erreur: {e}");
            Err(e.into())
        }
    }
}

async fn leg_anthropic() -> Result<()> {
    println!("=== leg ANTHROPIC (sonde du blocage outils tiers) ===");
    // Two possible credentials: API token (sk-ant-...) OR OAuth subscription token.
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let oauth = std::env::var("ANTHROPIC_OAUTH_TOKEN").ok();

    if api_key.is_none() && oauth.is_none() {
        println!("Aucune credential Anthropic en env. Pour compléter la sonde :");
        println!("  # token API (devrait marcher) :");
        println!("  ANTHROPIC_API_KEY=sk-ant-... cargo run -p s1-provider-access -- anthropic");
        println!("  # token d'abonnement Pro/Max (devrait être BLOQUÉ) :");
        println!("  ANTHROPIC_OAUTH_TOKEN=... cargo run -p s1-provider-access -- anthropic");
        println!(
            "[anthropic] leg NON exécuté. Hypothèse documentée: abonnement BLOQUÉ, token OK (ADR-7 R1)."
        );
        return Ok(());
    }

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-3-5-haiku-20241022",
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "Réponds: OK." }],
    });

    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    if let Some(k) = &api_key {
        req = req.header("x-api-key", k.as_str());
    } else if let Some(t) = &oauth {
        // subscription path: Authorization Bearer (the case Anthropic blocks)
        req = req
            .header("authorization", format!("Bearer {t}"))
            .header("anthropic-beta", "oauth-2025-04-20");
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

    println!("[anthropic] HTTP {status}");
    println!("[anthropic] body capturé:\n{text}\n");

    let class = classify_http(status, &text);
    println!("[anthropic] classe d'erreur: {class}");
    match class {
        "Auth::ThirdPartyBlocked" => {
            println!(
                "[anthropic] VERDICT: abonnement BLOQUÉ pour outil tiers (message exact capturé ci-dessus). \
                 Le MVP ne PEUT PAS en dépendre — confirme la stratégie model-agnostic (ADR-7 R1)."
            );
        }
        _ if status == 200 => {
            println!(
                "[anthropic] VERDICT: cette credential fonctionne (token API). Anthropic = provider conditionnel OK au token."
            );
        }
        _ => {
            println!(
                "[anthropic] VERDICT partiel: réponse non-200 sans message de blocage tiers — voir body."
            );
        }
    }
    Ok(())
}

/// Minimal classification (the full ErrorClass taxonomy belongs to US-015).
fn classify_http(status: u16, body: &str) -> &'static str {
    if body.contains("only authorized for use with Claude Code") {
        return "Auth::ThirdPartyBlocked";
    }
    match status {
        401 | 403 => "Auth::Invalid",
        429 => "RateLimited",
        529 => "Overloaded(529)",
        400 | 422 => "InvalidRequest",
        500..=599 => "Retryable",
        200..=299 => "Ok",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::classify_http;

    #[test]
    fn third_party_block_message_is_classified() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"This credential is only authorized for use with Claude Code and cannot be used for other API requests."}}"#;
        assert_eq!(classify_http(403, body), "Auth::ThirdPartyBlocked");
    }

    #[test]
    fn plain_401_is_invalid_auth() {
        assert_eq!(classify_http(401, "{}"), "Auth::Invalid");
    }

    #[test]
    fn overloaded_and_ratelimited() {
        assert_eq!(classify_http(529, "{}"), "Overloaded(529)");
        assert_eq!(classify_http(429, "{}"), "RateLimited");
    }
}
