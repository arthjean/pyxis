//! Provider-scoped inference credentials.
//!
//! This module is deliberately transport-neutral. It validates that a credential
//! belongs to the selected provider, materializes only the headers needed by that
//! provider, and exposes a non-secret fingerprint for catalog and connection scope.
//!
//! Header values are `Secret`, so a rendering redacts them because of what they
//! are rather than because every struct here remembered to hand-write a `Debug`.
//! The one hand-written `Debug` left is for an endpoint URL, whose query string
//! can carry a key that no field type can see.

use thiserror::Error;
use url::Url;

use crate::{ProviderId, Secret};

mod bedrock;
mod http;
mod validation;

/// A URL as it may be logged: no query, no fragment. Both of those routinely
/// carry an API key on OpenAI-compatible endpoints.
fn redacted_url(url: &str) -> String {
    Url::parse(url)
        .map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| "<invalid URL>".into())
}

/// Authorized HTTP request identity: where to send it and what to send with it.
#[derive(Clone)]
pub struct ProviderRequestAuth {
    pub url: String,
    pub headers: Vec<(String, Secret)>,
}

impl ProviderRequestAuth {
    /// Header names and values, for the transport that is about to send them.
    /// This is the point of use: past here the values are plain strings.
    pub fn header_pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.expose()))
    }
}

impl std::fmt::Debug for ProviderRequestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRequestAuth")
            .field("url", &redacted_url(&self.url))
            .field("headers", &self.headers)
            .finish()
    }
}

/// Authentication material accepted by model providers.
///
/// Every variant here has a caller. Five more used to exist (`ExperimentalBearer`,
/// `ChatGptOAuth`, `PersonalAccessToken`, `PrebuiltHeaders`, `AgentIdentity`) with
/// none: `ChatGptOAuth` in particular duplicated the ChatGPT path that
/// `oauth::openai_chatgpt::responses_request` already serves. Add a variant back
/// when something constructs it.
#[derive(Debug, Clone)]
pub enum ProviderCredential {
    /// For an endpoint configured with `OpenAiAuthPolicy::AllowUnauthenticated`,
    /// which is how a local OpenAI-compatible server with no key is reached.
    Unauthenticated,
    ApiKey {
        provider: ProviderId,
        key: Secret,
        /// Names the account this key belongs to. Absent: the key itself is
        /// hashed into the fingerprint, so rotating it reads as a new identity.
        identity: Option<String>,
    },
    BedrockApiKey {
        token: Secret,
        region: String,
        identity: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthKind {
    Unauthenticated,
    ApiKey,
}

/// The OpenAI-compatible boundary a credential is authorized to cross.
///
/// This was an enum with a single variant and a dispatcher with a single arm,
/// while Bedrock, the second provider, went around it entirely
/// ([`ProviderCredential::resolve_bedrock_api_key`]). A struct says the same
/// thing without promising a dispatch that does not exist.
#[derive(Clone)]
pub struct OpenAiAuthTarget {
    pub provider: ProviderId,
    pub endpoint: Url,
    pub allow_unauthenticated: bool,
}

impl std::fmt::Debug for OpenAiAuthTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiAuthTarget")
            .field("provider", &self.provider)
            .field(
                "endpoint_origin",
                &self.endpoint.origin().ascii_serialization(),
            )
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .finish()
    }
}

/// Region-scoped Bedrock API-key material for the AWS SDK bearer provider.
/// The token never passes through an HTTP-header representation.
#[derive(Debug, Clone)]
pub struct ResolvedBedrockAuth {
    token: Secret,
    pub identity_fingerprint: String,
}

impl ResolvedBedrockAuth {
    pub fn token(&self) -> &Secret {
        &self.token
    }
}

/// Materialized provider headers.
#[derive(Debug, Clone)]
pub struct ResolvedProviderAuth {
    pub provider: ProviderId,
    pub kind: ProviderAuthKind,
    pub identity_fingerprint: String,
    headers: Vec<(String, Secret)>,
}

impl ResolvedProviderAuth {
    pub fn headers(&self) -> &[(String, Secret)] {
        &self.headers
    }

    /// Turns resolved auth into a request spec for `endpoint`, appending the
    /// headers the transport needs. Callers used to assemble this themselves,
    /// each with its own idea of which content headers belong on it.
    pub fn into_request(
        self,
        endpoint: &Url,
        extra: impl IntoIterator<Item = (String, Secret)>,
    ) -> ProviderRequestAuth {
        let mut headers = self.headers;
        headers.extend(extra);
        ProviderRequestAuth {
            url: endpoint.to_string(),
            headers,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn openai(provider: ProviderId) -> OpenAiAuthTarget {
        OpenAiAuthTarget {
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
            .resolve_openai(&openai(ProviderId::OpenAiResponses))
            .unwrap();
        let (name, value) = &resolved.headers()[0];
        assert_eq!(name, "authorization");
        assert_eq!(value.expose(), "Bearer sk-secret");
        let debug = format!("{credential:?} {resolved:?}");
        assert!(!debug.contains("sk-secret"));
        assert!(debug.contains("authorization"));
        assert!(matches!(
            credential.resolve_openai(&openai(ProviderId::OpenAiChatGpt)),
            Err(ProviderAuthError::WrongProvider)
        ));
    }

    #[test]
    fn an_unauthenticated_credential_needs_the_policy_that_allows_it() {
        assert!(matches!(
            ProviderCredential::Unauthenticated
                .resolve_openai(&openai(ProviderId::OpenAiResponses)),
            Err(ProviderAuthError::AuthenticationRequired)
        ));
        let allowed = OpenAiAuthTarget {
            allow_unauthenticated: true,
            ..openai(ProviderId::OpenAiResponses)
        };
        let resolved = ProviderCredential::Unauthenticated
            .resolve_openai(&allowed)
            .unwrap();
        assert_eq!(resolved.kind, ProviderAuthKind::Unauthenticated);
        assert!(resolved.headers().is_empty());
    }

    /// The fallback that used to be silent: with no `identity`, the secret is
    /// what gets hashed. Two credentials must not collide just because one of
    /// them named an identity that happens to equal the other's key.
    #[test]
    fn a_named_identity_and_a_key_shaped_like_it_do_not_share_a_fingerprint() {
        let named = ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("other-key"),
            identity: Some("sk-secret".into()),
        };
        let anonymous = ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("sk-secret"),
            identity: None,
        };
        let target = openai(ProviderId::OpenAiResponses);
        assert_ne!(
            named.resolve_openai(&target).unwrap().identity_fingerprint,
            anonymous
                .resolve_openai(&target)
                .unwrap()
                .identity_fingerprint
        );
    }

    #[test]
    fn an_identity_that_could_not_be_logged_is_refused() {
        let credential = ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("sk-secret"),
            identity: Some("line\nbreak".into()),
        };
        let error = credential
            .resolve_openai(&openai(ProviderId::OpenAiResponses))
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderAuthError::InvalidField {
                field: "identity",
                ..
            }
        ));
    }

    #[test]
    fn bedrock_api_key_cannot_cross_into_openai() {
        let credential = ProviderCredential::BedrockApiKey {
            token: Secret::new("bedrock-secret"),
            region: "eu-west-3".into(),
            identity: None,
        };
        assert!(matches!(
            credential.resolve_openai(&openai(ProviderId::OpenAiResponses)),
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
        assert!(matches!(
            credential.resolve_bedrock_api_key("us-east-1"),
            Err(ProviderAuthError::InvalidField {
                field: "region",
                ..
            })
        ));
    }

    #[test]
    fn a_request_spec_hides_the_endpoint_query_and_fragment() {
        let request = ProviderRequestAuth {
            url: "https://example.test/responses?api-key=QUERY_SECRET#FRAGMENT_SECRET".into(),
            headers: vec![("x-api-key".into(), Secret::new("HEADER_SECRET"))],
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("QUERY_SECRET"));
        assert!(!debug.contains("FRAGMENT_SECRET"));
        assert!(!debug.contains("HEADER_SECRET"));
        assert!(debug.contains("x-api-key"));
        assert_eq!(
            request.header_pairs().collect::<Vec<_>>(),
            vec![("x-api-key", "HEADER_SECRET")]
        );
    }

    #[test]
    fn auth_target_debug_omits_endpoint_query_and_fragment() {
        let target = OpenAiAuthTarget {
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
