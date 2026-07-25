//! Management of the ChatGPT subscription OAuth credential for the adapter:
//! **rotating** refresh under a lock, keyring persistence, and building of the
//! inference headers (delegates to `agent-auth`).
//!
//! `Provider::stream` takes `&self` -> the credential lives behind a
//! `tokio::sync::Mutex` (interior mutability; a network refresh can happen under the lock).

use agent_auth::oauth::openai_chatgpt::{self, AuthError, RequestSpec};
use agent_auth::{Credential, OAuthCredential};
use agent_core::provider::ProviderError;

/// Refresh margin: we refresh 60 s BEFORE expiry to avoid an
/// expiry/request race (Pi aims at the exact edge; the margin is more robust).
const REFRESH_MARGIN_MS: u64 = 60_000;

pub struct CredentialManager {
    state: tokio::sync::Mutex<CredentialState>,
    http: reqwest::Client,
    /// Keyring key where the refreshed credential is rewritten (rotating refresh).
    keyring_account: String,
}

struct CredentialState {
    cred: Option<OAuthCredential>,
    persist_dirty: bool,
}

impl CredentialManager {
    pub fn new(
        cred: OAuthCredential,
        http: reqwest::Client,
        keyring_account: impl Into<String>,
    ) -> Self {
        Self {
            state: tokio::sync::Mutex::new(CredentialState {
                cred: Some(cred),
                persist_dirty: false,
            }),
            http,
            keyring_account: keyring_account.into(),
        }
    }

    /// Guarantees a fresh access token (refresh + keyring rewrite when needed)
    /// and returns the inference request spec (URL + proprietary headers).
    pub async fn request_spec(&self) -> Result<RequestSpec, ProviderError> {
        self.fresh_spec(openai_chatgpt::responses_request).await
    }

    /// Same as `request_spec` for the model catalog discovery (`/models`).
    pub async fn models_spec(&self) -> Result<RequestSpec, ProviderError> {
        self.fresh_spec(openai_chatgpt::models_request).await
    }

    /// Guarantees a fresh access token then builds the spec through `build`.
    async fn fresh_spec(
        &self,
        build: fn(&OAuthCredential) -> Result<RequestSpec, AuthError>,
    ) -> Result<RequestSpec, ProviderError> {
        let mut state = self.state.lock().await;
        let now = openai_chatgpt::now_ms();
        if state.cred.is_none() {
            return Err(disconnected_error());
        }
        if state.persist_dirty {
            let cred = state.cred.as_ref().ok_or_else(disconnected_error)?;
            self.persist(cred).await?;
            state.persist_dirty = false;
        }
        let cred = state.cred.as_mut().ok_or_else(disconnected_error)?;
        if now.saturating_add(REFRESH_MARGIN_MS) >= cred.expires_at {
            self.refresh_locked(&mut state, now).await?;
        }
        let cred = state.cred.as_ref().ok_or_else(disconnected_error)?;
        build(cred).map_err(convert_auth_err)
    }

    /// Forces a refresh even when the local clock still believes the token is valid.
    pub async fn force_refresh(&self) -> Result<(), ProviderError> {
        let mut state = self.state.lock().await;
        if state.cred.is_none() {
            return Err(disconnected_error());
        }
        self.refresh_locked(&mut state, openai_chatgpt::now_ms())
            .await
    }

    /// Invalidates the in-memory credential. Used by the interactive logout after
    /// the keyring deletion, to prevent a resurrection on the next refresh.
    pub async fn disconnect(&self) {
        let mut state = self.state.lock().await;
        state.cred = None;
        state.persist_dirty = false;
    }

    async fn refresh_locked(
        &self,
        state: &mut CredentialState,
        now: u64,
    ) -> Result<(), ProviderError> {
        let refresh_token = state
            .cred
            .as_ref()
            .ok_or_else(disconnected_error)?
            .refresh
            .expose()
            .to_string();
        let refreshed = openai_chatgpt::refresh(&self.http, &refresh_token, now)
            .await
            .map_err(convert_auth_err)?;
        state.cred = Some(refreshed.clone());
        state.persist_dirty = true;
        self.persist(&refreshed).await?;
        state.persist_dirty = false;
        Ok(())
    }

    /// Rewrites the refreshed credential into the keyring (blocking op -> outside the
    /// async runtime).
    async fn persist(&self, cred: &OAuthCredential) -> Result<(), ProviderError> {
        let account = self.keyring_account.clone();
        let blob = Credential::Oauth(cred.clone());
        tokio::task::spawn_blocking(move || agent_auth::store::save(&account, &blob))
            .await
            .map_err(|e| ProviderError::Transport(format!("join keyring: {e}")))?
            .map_err(|e| ProviderError::Transport(format!("keyring: {e}")))
    }
}

fn disconnected_error() -> ProviderError {
    ProviderError::Http {
        status: 401,
        message: "auth disconnected".to_string(),
        retry_after_ms: None,
    }
}

/// Maps an auth error into a `ProviderError` while preserving the retry
/// semantics: a refresh rejected with a 401/403 (revoked refresh / Codex client cut off) is
/// **fatal** (`Http` -> `Auth` on the `classify_error` side), not a transient retry.
fn convert_auth_err(e: AuthError) -> ProviderError {
    match e {
        AuthError::Http(re) => match re.status() {
            Some(s) if s.as_u16() == 401 || s.as_u16() == 403 => ProviderError::Http {
                status: s.as_u16(),
                message: "OAuth refresh rejected (revoked token?)".to_string(),
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
