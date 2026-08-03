//! Management of the ChatGPT subscription OAuth credential for the adapter:
//! **rotating** refresh under a lock, keyring persistence, and building of the
//! inference headers (delegates to `agent-auth`).
//!
//! `Provider::stream` takes `&self` -> the credential lives behind a
//! `tokio::sync::Mutex` (interior mutability; a network refresh can happen under the lock).

use agent_auth::oauth::{AuthError as OAuthError, now_ms, openai_chatgpt};
use agent_auth::provider::ProviderRequestAuth;
use agent_auth::{Credential, OAuthCredential};
use agent_core::provider::{AuthError, ProviderError};

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
    pub async fn request_spec(&self) -> Result<ProviderRequestAuth, ProviderError> {
        self.fresh_spec(openai_chatgpt::responses_request, false)
            .await
    }

    /// Catalog discovery runs outside a sampling and may refresh proactively.
    pub async fn models_spec(&self) -> Result<ProviderRequestAuth, ProviderError> {
        self.fresh_spec(openai_chatgpt::models_request, true).await
    }

    async fn fresh_spec(
        &self,
        build: fn(&OAuthCredential) -> Result<ProviderRequestAuth, OAuthError>,
        refresh_expiring: bool,
    ) -> Result<ProviderRequestAuth, ProviderError> {
        let mut state = self.state.lock().await;
        let now = now_ms();
        if state.cred.is_none() {
            return Err(disconnected_error());
        }
        if state.persist_dirty {
            let cred = state.cred.as_ref().ok_or_else(disconnected_error)?;
            self.persist(cred).await.map_err(convert_persist_err)?;
            state.persist_dirty = false;
        }
        let cred = state.cred.as_mut().ok_or_else(disconnected_error)?;
        // The margin lives with the credential type: refreshing 60 s early
        // rather than racing the exact edge is a property of the credential,
        // not of this manager.
        if cred.needs_refresh(now, agent_auth::REFRESH_MARGIN_MS) {
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
        self.refresh_locked(&mut state, now_ms())
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
        let refreshed = match openai_chatgpt::refresh(&self.http, &refresh_token, now).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let error = convert_refresh_err(error);
                if matches!(
                    error,
                    ProviderError::Credential(AuthError::RecoveryPermanent)
                ) {
                    state.cred = None;
                    state.persist_dirty = false;
                    if let Err(cleanup) = self.delete_persisted().await {
                        tracing::warn!(
                            target: "pyxis::auth",
                            error = %cleanup,
                            "persisted credential cleanup failed after permanent recovery failure"
                        );
                    }
                }
                return Err(error);
            }
        };
        state.cred = Some(refreshed.clone());
        state.persist_dirty = true;
        self.persist(&refreshed)
            .await
            .map_err(convert_persist_err)?;
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

    async fn delete_persisted(&self) -> Result<(), ProviderError> {
        let account = self.keyring_account.clone();
        tokio::task::spawn_blocking(move || agent_auth::store::delete(&account))
            .await
            .map_err(|error| ProviderError::Transport(format!("join keyring: {error}")))?
            .map_err(|error| ProviderError::Transport(format!("keyring: {error}")))
    }
}

fn disconnected_error() -> ProviderError {
    ProviderError::Credential(AuthError::ReconnectRequired)
}

/// A refresh attempted by the credential manager is already the one recovery
/// operation for this sampling. Its failure is typed terminal state, never a
/// synthetic 401 that could trigger a second refresh in the core.
fn convert_refresh_err(error: OAuthError) -> ProviderError {
    let outcome = match error {
        OAuthError::Http(error) => match error.status() {
            Some(status)
                if status == reqwest::StatusCode::REQUEST_TIMEOUT
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error() =>
            {
                AuthError::RecoveryTransient
            }
            Some(_) => AuthError::RecoveryPermanent,
            None => AuthError::RecoveryTransient,
        },
        // A timeout, an unreadable socket or an unavailable keyring say nothing
        // about the credential: the same refresh can succeed on the next try.
        OAuthError::Io(_) | OAuthError::Timeout { .. } | OAuthError::Store(_) => {
            AuthError::RecoveryTransient
        }
        // Everything else means this credential will not come back on its own.
        OAuthError::TokenResponse(_)
        | OAuthError::Jwt(_)
        | OAuthError::MissingAccountId
        | OAuthError::MissingRefreshToken { .. }
        | OAuthError::WrongProvider(_)
        | OAuthError::Callback(_)
        | OAuthError::StateMismatch
        | OAuthError::AuthorizationDenied(_)
        | OAuthError::DeviceTimeout
        | OAuthError::DeviceDenied(_)
        | OAuthError::InsecureEndpoint { .. }
        | OAuthError::Discovery(_)
        | OAuthError::ResponseTooLarge { .. } => AuthError::RecoveryPermanent,
    };
    ProviderError::Credential(outcome)
}

fn convert_persist_err(_error: ProviderError) -> ProviderError {
    ProviderError::Credential(AuthError::RecoveryTransient)
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

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn status_error(status: u16) -> OAuthError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 {status} fixture\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let error = reqwest::Client::new()
            .get(format!("http://{address}/token"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        server.await.unwrap();
        OAuthError::Http(error)
    }

    #[tokio::test]
    async fn refresh_failures_preserve_permanent_and_transient_outcomes() {
        assert!(matches!(
            convert_refresh_err(status_error(400).await),
            ProviderError::Credential(AuthError::RecoveryPermanent)
        ));
        assert!(matches!(
            convert_refresh_err(status_error(503).await),
            ProviderError::Credential(AuthError::RecoveryTransient)
        ));
        assert!(matches!(
            convert_refresh_err(OAuthError::TokenResponse("malformed".into())),
            ProviderError::Credential(AuthError::RecoveryPermanent)
        ));
        assert!(matches!(
            convert_persist_err(ProviderError::Transport("keyring".into())),
            ProviderError::Credential(AuthError::RecoveryTransient)
        ));
    }
}
