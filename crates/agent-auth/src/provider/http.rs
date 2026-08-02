use url::Url;

use crate::ProviderId;
use crate::oauth::openai_chatgpt::originator;

use super::validation::{
    add_fedramp_header, bearer_headers, fingerprint, invalid, validate_identity,
    validate_prebuilt_headers, validated_header_value,
};
use super::{ProviderAuthError, ProviderAuthKind, ProviderCredential, ResolvedProviderAuth};

impl ProviderCredential {
    pub(super) fn resolve_openai(
        &self,
        provider: ProviderId,
        endpoint: &Url,
        allow_unauthenticated: bool,
    ) -> Result<ResolvedProviderAuth, ProviderAuthError> {
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(invalid("endpoint", "expected an absolute HTTP(S) URL"));
        }

        let origin = endpoint.origin().ascii_serialization();
        let (credential_provider, kind, headers, identity) = match self {
            Self::Unauthenticated if allow_unauthenticated => (
                provider,
                ProviderAuthKind::Unauthenticated,
                Vec::new(),
                "none",
            ),
            Self::Unauthenticated => return Err(ProviderAuthError::AuthenticationRequired),
            Self::ApiKey {
                provider,
                key,
                identity,
            } => (
                *provider,
                ProviderAuthKind::ApiKey,
                bearer_headers(key)?,
                identity.as_deref().unwrap_or_else(|| key.expose()),
            ),
            Self::ExperimentalBearer {
                provider,
                token,
                identity,
            } => (
                *provider,
                ProviderAuthKind::ExperimentalBearer,
                bearer_headers(token)?,
                identity.as_deref().unwrap_or_else(|| token.expose()),
            ),
            Self::ChatGptOAuth {
                credential,
                fedramp,
            } => {
                let account_id = credential.account_id.as_deref().ok_or_else(|| {
                    invalid("account_id", "required for ChatGPT OAuth credentials")
                })?;
                let mut headers = bearer_headers(&credential.access)?;
                headers.push(("chatgpt-account-id".into(), account_id.into()));
                headers.push(("originator".into(), originator()));
                add_fedramp_header(&mut headers, *fedramp);
                (
                    credential.provider,
                    ProviderAuthKind::ChatGptOAuth,
                    headers,
                    account_id,
                )
            }
            Self::PersonalAccessToken {
                provider,
                token,
                account_id,
                fedramp,
            } => {
                validate_identity("account_id", account_id)?;
                let mut headers = bearer_headers(token)?;
                headers.push(("chatgpt-account-id".into(), account_id.clone()));
                add_fedramp_header(&mut headers, *fedramp);
                (
                    *provider,
                    ProviderAuthKind::PersonalAccessToken,
                    headers,
                    account_id.as_str(),
                )
            }
            Self::PrebuiltHeaders {
                provider,
                headers,
                identity,
            } => {
                validate_identity("identity", identity)?;
                (
                    *provider,
                    ProviderAuthKind::PrebuiltHeaders,
                    validate_prebuilt_headers(headers)?,
                    identity.as_str(),
                )
            }
            Self::AgentIdentity {
                provider,
                authorization,
                account_id,
                fedramp,
                identity,
            } => {
                validate_identity("account_id", account_id)?;
                validate_identity("identity", identity)?;
                let authorization =
                    validated_header_value("authorization", authorization.expose())?;
                let mut headers = vec![
                    ("authorization".into(), authorization),
                    ("chatgpt-account-id".into(), account_id.clone()),
                ];
                add_fedramp_header(&mut headers, *fedramp);
                (
                    *provider,
                    ProviderAuthKind::AgentIdentity,
                    headers,
                    identity.as_str(),
                )
            }
            Self::BedrockApiKey { .. } => {
                return Err(ProviderAuthError::UnsupportedCredential);
            }
        };

        if credential_provider != provider {
            return Err(ProviderAuthError::WrongProvider);
        }
        Ok(ResolvedProviderAuth {
            provider,
            kind,
            identity_fingerprint: fingerprint(&[
                &format!("{provider:?}"),
                &origin,
                &format!("{kind:?}"),
                identity,
            ]),
            headers,
        })
    }
}
