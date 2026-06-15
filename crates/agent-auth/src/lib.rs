//! `agent-auth` — credentials BYOK + flows OAuth subscription, stockés dans le
//! secret store OS (US-018). Headless : aucune dépendance TUI/HTTP-serveur lourde.
//!
//! Couvre deux familles de credentials derrière une interface unique :
//! - `Credential::ApiKey` — BYOK au token (OpenAI Chat US-017, Gemini, OpenRouter…).
//! - `Credential::Oauth`  — OAuth subscription (Anthropic, abonnement ChatGPT ADR-10).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod oauth;
pub mod store;

use serde::{Deserialize, Serialize};

/// Identifiant de provider (sous-ensemble Phase 1). Cible MVP = `OpenAiChatGpt`
/// (abonnement). Les autres s'ajouteront ensuite (Ollama retiré du scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// Abonnement ChatGPT, Responses API sur le backend ChatGPT (ADR-10) — MVP.
    OpenAiChatGpt,
    /// Chat Completions au token, BYOK (provider futur).
    OpenAiChat,
    Anthropic,
}

/// Secret en mémoire. Son `Debug` est expurgé (jamais de token en logs) ; il ne
/// se sérialise en clair QUE vers le secret store OS (jamais sur disque).
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// Expose la valeur (à n'utiliser qu'au point d'usage : header, body).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Credential d'un provider, telle que stockée dans le keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    ApiKey { provider: ProviderId, key: Secret },
    Oauth(OAuthCredential),
}

/// Credential OAuth (sliding refresh). `account_id` porte le `chatgpt_account_id`
/// pour l'abonnement ChatGPT (requis pour router) ; `None` pour Anthropic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub provider: ProviderId,
    pub access: Secret,
    pub refresh: Secret,
    /// timestamp ms absolu (cf. Pi `expires`).
    pub expires_at: u64,
    pub account_id: Option<String>,
}

impl OAuthCredential {
    /// Expiré à `now_ms` ? Bord exact, sans marge (comme Pi côté OpenAI).
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at
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
        // tokens présents en clair dans le blob (destiné au keyring chiffré par l'OS)
        assert!(blob.contains("\"at\"") && blob.contains("\"rt\""));
        assert!(blob.contains("\"kind\":\"oauth\""));
        let back: Credential = serde_json::from_str(&blob).unwrap();
        let Credential::Oauth(o) = back else {
            unreachable!("variante oauth attendue")
        };
        assert_eq!(o.account_id.as_deref(), Some("acct_1"));
        assert!(o.is_expired(1_000));
        assert!(!o.is_expired(999));
    }
}
