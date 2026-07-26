//! ChatGPT subscription login (PKCE browser flow) -> keyring storage.
//!
//! `cargo run -p agent-auth --example login`
//!
//! Opens the browser on auth.openai.com, waits for the local callback
//! (127.0.0.1:1455), exchanges the code, and writes the OAuth credential into the
//! OS keyring under the key `oauth:openai_chatgpt`. The adapter smoke test
//! (`agent-provider --example smoke`) then reads it back.

use agent_auth::oauth::openai_chatgpt;
use agent_auth::{Credential, store};

const ACCOUNT: &str = "oauth:openai_chatgpt";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    println!("Authorize Pyxis with your ChatGPT subscription...");
    let cred = openai_chatgpt::login_browser_with_notice(&client, |url, opened| {
        if !opened {
            println!("Open this URL to authorize Pyxis:\n{url}");
        }
    })
    .await?;
    store::save(ACCOUNT, &Credential::Oauth(cred))?;
    println!("Connected. Credential stored in the keyring ({ACCOUNT}).");
    println!("Smoke test: cargo run -p agent-provider --example smoke -- \"say hello\"");
    Ok(())
}
