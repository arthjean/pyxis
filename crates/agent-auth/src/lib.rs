//! `agent-auth`: BYOK credentials + OAuth subscription flows, stored in the
//! OS secret store (US-018). Headless: no heavy TUI/HTTP-server dependency.
//!
//! Covers two families of credentials behind a single interface:
//! - `Credential::ApiKey`: token-based BYOK (OpenAI Chat US-017, Gemini, OpenRouter, ...).
//! - `Credential::Oauth`: OAuth subscription (Anthropic, ChatGPT subscription ADR-10).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod oauth;
pub mod provider;
pub mod store;

use serde::{Deserialize, Serialize};

/// Provider identifier (Phase 1 subset). MVP target = `OpenAiChatGpt`
/// (subscription). The others will be added later (Ollama dropped from the scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// ChatGPT subscription, Responses API on the ChatGPT backend (ADR-10): MVP.
    OpenAiChatGpt,
    /// Token-based Chat Completions, BYOK (future provider).
    OpenAiChat,
    /// Configured OpenAI-compatible Responses endpoint.
    OpenAiResponses,
    /// Amazon Bedrock Runtime.
    AmazonBedrock,
    Anthropic,
}

/// In-memory secret. Its `Debug` is redacted (never a token in the logs); it
/// serializes in clear text ONLY toward the OS secret store (never to disk).
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// Exposes the value (to be used only at the point of use: header, body).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Credential of a provider, as stored in the keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        provider: ProviderId,
        key: Secret,
    },
    Oauth(OAuthCredential),
    /// Per-server MCP authorization (MCP spec 2025-06-18). Kept apart from
    /// `Oauth` because it belongs to a named server rather than to a provider,
    /// and it carries what a refresh needs without redoing discovery.
    McpOauth(oauth::mcp::McpOAuthCredential),
}

/// How far ahead of an expiry a token is refreshed rather than raced.
///
/// One definition for every OAuth credential in the workspace. It used to exist
/// three times: exact-edge here, with a margin on the MCP side, and a third copy
/// inlined in the provider crate that consumes this one.
pub const REFRESH_MARGIN_MS: u64 = 60_000;

/// OAuth credential (sliding refresh). `account_id` carries the `chatgpt_account_id`
/// for the ChatGPT subscription (required for routing); `None` for Anthropic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub provider: ProviderId,
    pub access: Secret,
    pub refresh: Secret,
    /// absolute ms timestamp (see Pi's `expires`).
    pub expires_at: u64,
    pub account_id: Option<String>,
}

impl OAuthCredential {
    /// Is this credential expired at `now_ms`, or close enough that a call would
    /// race the expiry? Pass `0` for the exact edge.
    pub fn needs_refresh(&self, now_ms: u64, margin_ms: u64) -> bool {
        now_ms.saturating_add(margin_ms) >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("sk-super-secret");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "sk-super-secret");
    }

    #[test]
    fn oauth_credential_roundtrips_through_json() {
        let cred = Credential::Oauth(OAuthCredential {
            provider: ProviderId::OpenAiChatGpt,
            access: Secret::new("at"),
            refresh: Secret::new("rt"),
            expires_at: 1_000,
            account_id: Some("acct_1".into()),
        });
        let blob = serde_json::to_string(&cred).unwrap();
        // tokens present in clear text in the blob (destined for the OS-encrypted keyring)
        assert!(blob.contains("\"at\"") && blob.contains("\"rt\""));
        assert!(blob.contains("\"kind\":\"oauth\""));
        let back: Credential = serde_json::from_str(&blob).unwrap();
        let Credential::Oauth(o) = back else {
            unreachable!("expected oauth variant")
        };
        assert_eq!(o.account_id.as_deref(), Some("acct_1"));
        assert!(o.needs_refresh(1_000, 0));
        assert!(!o.needs_refresh(999, 0));
        // The margin is what turns "expired" into "about to expire".
        assert!(o.needs_refresh(999, 1));
    }
}
