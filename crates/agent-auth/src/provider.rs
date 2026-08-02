//! Provider-scoped inference credentials.
//!
//! This module is deliberately transport-neutral. It validates that a credential
//! belongs to the selected provider, materializes only the headers needed by that
//! provider, and exposes a non-secret fingerprint for catalog and connection scope.

use thiserror::Error;
use url::Url;

use crate::{OAuthCredential, ProviderId, Secret};

mod bedrock;
mod http;
mod validation;

/// Authorized HTTP request identity. Values are materialized only at the
/// provider boundary and are always redacted from diagnostics.
#[derive(Clone)]
pub struct ProviderRequestAuth {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for ProviderRequestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_url = Url::parse(&self.url)
            .map(|mut url| {
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|_| "<invalid URL>".into());
        let header_names = self
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        f.debug_struct("ProviderRequestAuth")
            .field("url", &redacted_url)
            .field("header_names", &header_names)
            .field("header_values", &"Secret(***)")
            .finish()
    }
}

/// Authentication material accepted by model providers.
#[derive(Clone)]
pub enum ProviderCredential {
    Unauthenticated,
    ApiKey {
        provider: ProviderId,
        key: Secret,
        identity: Option<String>,
    },
    ExperimentalBearer {
        provider: ProviderId,
        token: Secret,
        identity: Option<String>,
    },
    ChatGptOAuth {
        credential: OAuthCredential,
        fedramp: bool,
    },
    PersonalAccessToken {
        provider: ProviderId,
        token: Secret,
        account_id: String,
        fedramp: bool,
    },
    PrebuiltHeaders {
        provider: ProviderId,
        headers: Vec<(String, Secret)>,
        identity: String,
    },
    AgentIdentity {
        provider: ProviderId,
        authorization: Secret,
        account_id: String,
        fedramp: bool,
        identity: String,
    },
    BedrockApiKey {
        token: Secret,
        region: String,
        identity: Option<String>,
    },
}

impl std::fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("ProviderCredential::Unauthenticated"),
            Self::ApiKey {
                provider, identity, ..
            } => f
                .debug_struct("ProviderCredential::ApiKey")
                .field("provider", provider)
                .field("has_identity", &identity.is_some())
                .finish_non_exhaustive(),
            Self::ExperimentalBearer {
                provider, identity, ..
            } => f
                .debug_struct("ProviderCredential::ExperimentalBearer")
                .field("provider", provider)
                .field("has_identity", &identity.is_some())
                .finish_non_exhaustive(),
            Self::ChatGptOAuth {
                credential,
                fedramp,
            } => f
                .debug_struct("ProviderCredential::ChatGptOAuth")
                .field("provider", &credential.provider)
                .field("account_id", &credential.account_id.as_ref().map(|_| "***"))
                .field("fedramp", fedramp)
                .finish_non_exhaustive(),
            Self::PersonalAccessToken {
                provider, fedramp, ..
            } => f
                .debug_struct("ProviderCredential::PersonalAccessToken")
                .field("provider", provider)
                .field("fedramp", fedramp)
                .finish_non_exhaustive(),
            Self::PrebuiltHeaders {
                provider,
                headers,
                identity: _,
            } => f
                .debug_struct("ProviderCredential::PrebuiltHeaders")
                .field("provider", provider)
                .field(
                    "header_names",
                    &headers.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                )
                .field("identity", &"***")
                .finish(),
            Self::AgentIdentity {
                provider, fedramp, ..
            } => f
                .debug_struct("ProviderCredential::AgentIdentity")
                .field("provider", provider)
                .field("fedramp", fedramp)
                .field("identity", &"***")
                .finish_non_exhaustive(),
            Self::BedrockApiKey {
                region, identity, ..
            } => f
                .debug_struct("ProviderCredential::BedrockApiKey")
                .field("region", region)
                .field("has_identity", &identity.is_some())
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthKind {
    Unauthenticated,
    ApiKey,
    ExperimentalBearer,
    ChatGptOAuth,
    PersonalAccessToken,
    PrebuiltHeaders,
    AgentIdentity,
}

/// The provider boundary a credential is authorized to cross.
#[derive(Clone)]
pub enum ProviderAuthTarget {
    OpenAi {
        provider: ProviderId,
        endpoint: Url,
        allow_unauthenticated: bool,
    },
}

impl std::fmt::Debug for ProviderAuthTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenAi {
                provider,
                endpoint,
                allow_unauthenticated,
            } => f
                .debug_struct("ProviderAuthTarget::OpenAi")
                .field("provider", provider)
                .field("endpoint_origin", &endpoint.origin().ascii_serialization())
                .field("allow_unauthenticated", allow_unauthenticated)
                .finish(),
        }
    }
}

/// Region-scoped Bedrock API-key material for the AWS SDK bearer provider.
/// The token never passes through an HTTP-header representation.
#[derive(Clone)]
pub struct ResolvedBedrockAuth {
    token: Secret,
    pub identity_fingerprint: String,
}

impl ResolvedBedrockAuth {
    pub fn token(&self) -> &Secret {
        &self.token
    }
}

impl std::fmt::Debug for ResolvedBedrockAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedBedrockAuth")
            .field("token", &"Secret(***)")
            .field("identity_fingerprint", &self.identity_fingerprint)
            .finish()
    }
}

/// Materialized provider headers. Debug output contains names, never values.
#[derive(Clone)]
pub struct ResolvedProviderAuth {
    pub provider: ProviderId,
    pub kind: ProviderAuthKind,
    pub identity_fingerprint: String,
    headers: Vec<(String, String)>,
}

impl ResolvedProviderAuth {
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

impl std::fmt::Debug for ResolvedProviderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProviderAuth")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("identity_fingerprint", &self.identity_fingerprint)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderAuthError {
    #[error("invalid provider auth field `{field}`: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("credential provider does not match the selected provider")]
    WrongProvider,
    #[error("authentication is required by the selected provider")]
    AuthenticationRequired,
    #[error("credential type is not supported by the selected provider")]
    UnsupportedCredential,
}

impl ProviderCredential {
    pub fn resolve(
        &self,
        target: &ProviderAuthTarget,
    ) -> Result<ResolvedProviderAuth, ProviderAuthError> {
        let ProviderAuthTarget::OpenAi {
            provider,
            endpoint,
            allow_unauthenticated,
        } = target;
        self.resolve_openai(*provider, endpoint, *allow_unauthenticated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai(provider: ProviderId) -> ProviderAuthTarget {
        ProviderAuthTarget::OpenAi {
            provider,
            endpoint: Url::parse("https://example.test/v1/responses").unwrap(),
            allow_unauthenticated: false,
        }
    }

    #[test]
    fn api_key_is_scoped_and_redacted() {
        let credential = ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("sk-secret"),
            identity: None,
        };
        let resolved = credential
            .resolve(&openai(ProviderId::OpenAiResponses))
            .unwrap();
        assert_eq!(
            resolved.headers()[0],
            ("authorization".into(), "Bearer sk-secret".into())
        );
        let debug = format!("{credential:?} {resolved:?}");
        assert!(!debug.contains("sk-secret"));
        assert!(matches!(
            credential.resolve(&openai(ProviderId::OpenAiChatGpt)),
            Err(ProviderAuthError::WrongProvider)
        ));
    }

    #[test]
    fn chatgpt_modes_attach_only_scoped_account_headers() {
        let credential = ProviderCredential::PersonalAccessToken {
            provider: ProviderId::OpenAiChatGpt,
            token: Secret::new("pat-secret"),
            account_id: "acct-secret".into(),
            fedramp: true,
        };
        let resolved = credential
            .resolve(&openai(ProviderId::OpenAiChatGpt))
            .unwrap();
        assert_eq!(resolved.headers().len(), 3);
        assert!(
            resolved
                .headers()
                .iter()
                .any(|(name, value)| { name == "x-openai-fedramp" && value == "true" })
        );
        assert!(!format!("{credential:?} {resolved:?}").contains("acct-secret"));
    }

    #[test]
    fn forbidden_prebuilt_header_names_the_field_without_the_value() {
        let credential = ProviderCredential::PrebuiltHeaders {
            provider: ProviderId::OpenAiResponses,
            headers: vec![("host".into(), Secret::new("secret.test"))],
            identity: "fixture".into(),
        };
        let error = credential
            .resolve(&openai(ProviderId::OpenAiResponses))
            .unwrap_err();
        assert!(error.to_string().contains("headers"));
        assert!(error.to_string().contains("host"));
        assert!(!error.to_string().contains("secret.test"));
    }

    #[test]
    fn bedrock_api_key_cannot_cross_into_openai() {
        let credential = ProviderCredential::BedrockApiKey {
            token: Secret::new("bedrock-secret"),
            region: "eu-west-3".into(),
            identity: None,
        };
        assert!(matches!(
            credential.resolve(&openai(ProviderId::OpenAiResponses)),
            Err(ProviderAuthError::UnsupportedCredential)
        ));
    }

    #[test]
    fn bedrock_api_key_is_region_scoped_and_redacted() {
        let credential = ProviderCredential::BedrockApiKey {
            token: Secret::new("bedrock-secret"),
            region: "eu-west-3".into(),
            identity: Some("bedrock-account".into()),
        };
        let resolved = credential.resolve_bedrock_api_key("eu-west-3").unwrap();
        assert_eq!(resolved.token().expose(), "bedrock-secret");
        let debug = format!("{credential:?} {resolved:?}");
        assert!(!debug.contains("bedrock-secret"));
        assert!(!debug.contains("bedrock-account"));
        assert!(matches!(
            credential.resolve_bedrock_api_key("us-east-1"),
            Err(ProviderAuthError::InvalidField {
                field: "region",
                ..
            })
        ));
    }

    #[test]
    fn every_openai_auth_mode_projects_only_its_scoped_headers() {
        let target = openai(ProviderId::OpenAiChatGpt);
        let cases = vec![
            ProviderCredential::ExperimentalBearer {
                provider: ProviderId::OpenAiChatGpt,
                token: Secret::new("experimental-secret"),
                identity: Some("experimental".into()),
            },
            ProviderCredential::ChatGptOAuth {
                credential: OAuthCredential {
                    provider: ProviderId::OpenAiChatGpt,
                    access: Secret::new("oauth-secret"),
                    refresh: Secret::new("refresh-secret"),
                    expires_at: u64::MAX,
                    account_id: Some("oauth-account".into()),
                },
                fedramp: true,
            },
            ProviderCredential::PrebuiltHeaders {
                provider: ProviderId::OpenAiChatGpt,
                headers: vec![("x-provider-auth".into(), Secret::new("header-secret"))],
                identity: "prebuilt".into(),
            },
            ProviderCredential::AgentIdentity {
                provider: ProviderId::OpenAiChatGpt,
                authorization: Secret::new("Bearer agent-secret"),
                account_id: "agent-account".into(),
                fedramp: true,
                identity: "agent-identity".into(),
            },
        ];
        for credential in cases {
            let resolved = credential.resolve(&target).unwrap();
            let debug = format!("{credential:?} {resolved:?}");
            for secret in [
                "experimental-secret",
                "oauth-secret",
                "refresh-secret",
                "oauth-account",
                "header-secret",
                "prebuilt",
                "agent-secret",
                "agent-account",
                "agent-identity",
            ] {
                assert!(!debug.contains(secret));
            }
            assert!(
                resolved
                    .headers()
                    .iter()
                    .all(|(name, _)| !matches!(name.as_str(), "host" | "content-length"))
            );
        }
    }

    #[test]
    fn auth_target_debug_omits_endpoint_query_and_fragment() {
        let target = ProviderAuthTarget::OpenAi {
            provider: ProviderId::OpenAiResponses,
            endpoint: Url::parse(
                "https://example.test/v1/responses?api-key=QUERY_SECRET#FRAGMENT_SECRET",
            )
            .unwrap(),
            allow_unauthenticated: false,
        };
        let debug = format!("{target:?}");
        assert!(!debug.contains("QUERY_SECRET"));
        assert!(!debug.contains("FRAGMENT_SECRET"));
        assert!(debug.contains("https://example.test"));
    }
}
