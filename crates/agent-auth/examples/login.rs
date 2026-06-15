//! Login abonnement ChatGPT (flow navigateur PKCE) → stockage keyring.
//!
//! `cargo run -p agent-auth --example login`
//!
//! Ouvre le navigateur sur auth.openai.com, attend le callback local
//! (127.0.0.1:1455), échange le code, et écrit la credential OAuth dans le
//! keyring OS sous la clé `oauth:openai_chatgpt`. Le smoke test de l'adapter
//! (`agent-provider --example smoke`) la relit ensuite.

use agent_auth::oauth::openai_chatgpt;
use agent_auth::{Credential, store};

const ACCOUNT: &str = "oauth:openai_chatgpt";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    println!("Autorisation de Numen via ton abonnement ChatGPT…");
    let cred = openai_chatgpt::login_browser(&client).await?;
    store::save(ACCOUNT, &Credential::Oauth(cred))?;
    println!("Connecté. Credential stockée dans le keyring ({ACCOUNT}).");
    println!("Smoke test : cargo run -p agent-provider --example smoke -- \"dis bonjour\"");
    Ok(())
}
