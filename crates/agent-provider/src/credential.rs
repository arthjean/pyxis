//! Management of the ChatGPT subscription OAuth credential for the adapter:
//! **rotating** refresh under a lock, keyring persistence, and building of the
//! inference headers (delegates to `agent-auth`).
//!
//! `Provider::stream` takes `&self` -> the credential lives behind a
//! `tokio::sync::Mutex` (interior mutability; a network refresh can happen under the lock).

use agent_auth::oauth::openai_chatgpt::{self, AuthError as OAuthError, RequestSpec};
use agent_auth::{Credential, OAuthCredential};
use agent_core::provider::{AuthError, ProviderError};

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

    /// Builds an inference request spec. A locally expiring token is surfaced
    /// to the core so its one refresh is observable and cancellation-guarded.
    pub async fn request_spec(&self) -> Result<RequestSpec, ProviderError> {
        self.fresh_spec(openai_chatgpt::responses_request, false)
            .await
    }

    /// Catalog discovery runs outside a sampling and may refresh proactively.
    pub async fn models_spec(&self) -> Result<RequestSpec, ProviderError> {
        self.fresh_spec(openai_chatgpt::models_request, true).await
    }

    async fn fresh_spec(
        &self,
        build: fn(&OAuthCredential) -> Result<RequestSpec, OAuthError>,
        refresh_expiring: bool,
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
            if !refresh_expiring {
                return Err(ProviderError::Credential(AuthError::Expired));
            }
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
            .map_err(convert_refresh_err)?;
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
    ProviderError::Credential(AuthError::ReconnectRequired)
}

/// A refresh attempted by the credential manager is already the one recovery
/// operation for this sampling. Its failure is typed terminal state, never a
/// synthetic 401 that could trigger a second refresh in the core.
fn convert_refresh_err(_error: OAuthError) -> ProviderError {
    ProviderError::Credential(AuthError::ReconnectRequired)
}

fn convert_auth_err(e: OAuthError) -> ProviderError {
    match e {
        OAuthError::Http(re) => match re.status() {
            Some(s) if s.as_u16() == 401 || s.as_u16() == 403 => {
                ProviderError::Credential(AuthError::ReconnectRequired)
            }
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
