use agent_auth::provider::{
    OpenAiAuthTarget, ProviderAuthError, ProviderAuthKind, ProviderCredential, ProviderRequestAuth,
    ResolvedProviderAuth,
};
use agent_core::provider::{AuthError, ProviderError};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::ConfiguredOpenAiProvider;
use crate::chatgpt_error::invalid_request;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiAccountState {
    pub provider_name: String,
    pub endpoint_origin: String,
    pub identity_fingerprint: String,
    pub auth_kind: ProviderAuthKind,
}

#[async_trait]
pub trait AuthRecovery: Send + Sync {
    async fn recover(&self, account: &OpenAiAccountState) -> Result<ProviderCredential, AuthError>;
}

impl ConfiguredOpenAiProvider {
    pub async fn account_state(&self) -> Result<OpenAiAccountState, ProviderError> {
        self.resolved_auth().await.map(|(state, _)| state)
    }

    pub async fn replace_credential(
        &self,
        credential: ProviderCredential,
    ) -> Result<OpenAiAccountState, ProviderError> {
        let (state, _) = self.resolve_credential(&credential)?;
        *self.credential.write().await = Some(credential);
        self.invalidate_scope();
        Ok(state)
    }

    pub(super) async fn resolved_auth(
        &self,
    ) -> Result<(OpenAiAccountState, ResolvedProviderAuth), ProviderError> {
        let credential = self
            .credential
            .read()
            .await
            .clone()
            .ok_or(ProviderError::Credential(AuthError::RecoveryUnavailable))?;
        self.resolve_credential(&credential)
    }

    fn resolve_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<(OpenAiAccountState, ResolvedProviderAuth), ProviderError> {
        let endpoint = self.config.endpoint()?;
        let auth = credential
            .resolve_openai(&OpenAiAuthTarget {
                provider: self.config.auth_provider,
                endpoint: endpoint.clone(),
                allow_unauthenticated: self.config.allow_unauthenticated(),
            })
            .map_err(auth_config_error)?;
        let state = OpenAiAccountState {
            provider_name: self.config.name.clone(),
            endpoint_origin: endpoint.origin().ascii_serialization(),
            identity_fingerprint: auth.identity_fingerprint.clone(),
            auth_kind: auth.kind,
        };
        Ok((state, auth))
    }

    pub(super) fn request_auth(
        endpoint: &url::Url,
        auth: ResolvedProviderAuth,
    ) -> ProviderRequestAuth {
        auth.into_request(
            endpoint,
            [
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
            ],
        )
    }

    pub(super) fn invalidate_scope(&self) {
        {
            let mut token = self
                .scope_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            token.cancel();
            *token = CancellationToken::new();
        }
        self.websocket.reset_scope();
        if let Err(error) = self.catalog_manager.invalidate_scope() {
            tracing::warn!(target: "pyxis::models", error = %error, "model scope invalidation failed");
        }
    }
}

pub(super) fn auth_config_error(error: ProviderAuthError) -> ProviderError {
    invalid_request(format!("invalid provider field `auth`: {error}"))
}
