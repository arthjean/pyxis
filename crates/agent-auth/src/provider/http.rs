use super::validation::{
    IdentitySource, bearer_headers, identity_fingerprint, invalid, validate_identity,
};
use super::{
    OpenAiAuthTarget, ProviderAuthError, ProviderAuthKind, ProviderCredential, ResolvedProviderAuth,
};

impl ProviderCredential {
    /// Materializes the headers this credential is allowed to send to `target`.
    pub fn resolve_openai(
        &self,
        target: &OpenAiAuthTarget,
    ) -> Result<ResolvedProviderAuth, ProviderAuthError> {
        let OpenAiAuthTarget {
            provider,
            endpoint,
            allow_unauthenticated,
        } = target;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(invalid("endpoint", "expected an absolute HTTP(S) URL"));
        }

        let (credential_provider, kind, headers, identity) = match self {
            Self::Unauthenticated if *allow_unauthenticated => (
                *provider,
                ProviderAuthKind::Unauthenticated,
                Vec::new(),
                IdentitySource::Anonymous,
            ),
            Self::Unauthenticated => return Err(ProviderAuthError::AuthenticationRequired),
            Self::ApiKey {
                provider,
                key,
                identity,
            } => {
                validate_identity("identity", identity.as_deref())?;
                (
                    *provider,
                    ProviderAuthKind::ApiKey,
                    bearer_headers(key)?,
                    IdentitySource::of(identity.as_deref(), key),
                )
            }
            Self::BedrockApiKey { .. } => return Err(ProviderAuthError::UnsupportedCredential),
        };

        if credential_provider != *provider {
            return Err(ProviderAuthError::WrongProvider);
        }
        Ok(ResolvedProviderAuth {
            provider: *provider,
            kind,
            identity_fingerprint: identity_fingerprint(
                &[
                    &format!("{provider:?}"),
                    &endpoint.origin().ascii_serialization(),
                    &format!("{kind:?}"),
                ],
                identity,
            ),
            headers,
        })
    }
}
