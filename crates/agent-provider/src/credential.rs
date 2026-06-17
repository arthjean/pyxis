//! Gestion de la credential OAuth de l'abonnement ChatGPT pour l'adapter :
//! refresh **rotatif** sous verrou, persistance keyring, et fabrication des
//! en-têtes d'inférence (délègue à `agent-auth`).
//!
//! `Provider::stream` prend `&self` → la credential vit derrière un
//! `tokio::sync::Mutex` (interior mutability ; refresh réseau possible sous lock).

use agent_auth::oauth::openai_chatgpt::{self, AuthError, RequestSpec};
use agent_auth::{Credential, OAuthCredential};
use agent_core::provider::ProviderError;

/// Marge de refresh : on rafraîchit 60 s AVANT l'expiration pour éviter une course
/// expiry/requête (Pi vise le bord exact ; la marge est plus robuste).
const REFRESH_MARGIN_MS: u64 = 60_000;

pub struct CredentialManager {
    cred: tokio::sync::Mutex<OAuthCredential>,
    http: reqwest::Client,
    /// Clé keyring où réécrire la credential rafraîchie (refresh rotatif).
    keyring_account: String,
}

impl CredentialManager {
    pub fn new(
        cred: OAuthCredential,
        http: reqwest::Client,
        keyring_account: impl Into<String>,
    ) -> Self {
        Self {
            cred: tokio::sync::Mutex::new(cred),
            http,
            keyring_account: keyring_account.into(),
        }
    }

    /// Garantit un access token frais (refresh + réécriture keyring si nécessaire)
    /// et retourne la spec de requête d'inférence (URL + en-têtes propriétaires).
    pub async fn request_spec(&self) -> Result<RequestSpec, ProviderError> {
        let mut cred = self.cred.lock().await;
        let now = openai_chatgpt::now_ms();
        if now.saturating_add(REFRESH_MARGIN_MS) >= cred.expires_at {
            let refreshed = openai_chatgpt::refresh(&self.http, cred.refresh.expose(), now)
                .await
                .map_err(convert_auth_err)?;
            self.persist(&refreshed).await?;
            *cred = refreshed;
        }
        openai_chatgpt::responses_request(&cred).map_err(convert_auth_err)
    }

    /// Réécrit la credential rafraîchie dans le keyring (op bloquante → hors
    /// runtime async).
    async fn persist(&self, cred: &OAuthCredential) -> Result<(), ProviderError> {
        let account = self.keyring_account.clone();
        let blob = Credential::Oauth(cred.clone());
        tokio::task::spawn_blocking(move || agent_auth::store::save(&account, &blob))
            .await
            .map_err(|e| ProviderError::Transport(format!("join keyring: {e}")))?
            .map_err(|e| ProviderError::Transport(format!("keyring: {e}")))
    }
}

/// Mappe une erreur d'auth vers `ProviderError` en préservant la sémantique de
/// retry : un refresh rejeté en 401/403 (refresh révoqué / client Codex coupé) est
/// **fatal** (`Http` → `Auth` côté `classify_error`), pas un retry transitoire.
fn convert_auth_err(e: AuthError) -> ProviderError {
    match e {
        AuthError::Http(re) => match re.status() {
            Some(s) if s.as_u16() == 401 || s.as_u16() == 403 => ProviderError::Http {
                status: s.as_u16(),
                message: "refresh OAuth rejeté (token révoqué ?)".to_string(),
                retry_after_ms: None,
            },
            Some(s) => ProviderError::Http {
                status: s.as_u16(),
                message: re.to_string(),
                retry_after_ms: None,
            },
            None => ProviderError::Transport(re.to_string()),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}
