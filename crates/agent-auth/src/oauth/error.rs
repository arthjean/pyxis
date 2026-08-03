//! The one error type every OAuth flow in this crate speaks.
//!
//! It used to live inside `openai_chatgpt`, which meant the MCP flow imported
//! its error vocabulary from an unrelated provider and dumped a dozen distinct
//! failures into `Callback(String)`. Each of those failures now has a variant,
//! so a caller can tell a discovery refusal from a timed-out socket without
//! matching on message text.

use crate::ProviderId;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("secret store: {0}")]
    Store(#[from] crate::store::StoreError),

    /// A bounded request did not answer in time. `operation` names which one.
    #[error("{operation} timed out")]
    Timeout { operation: &'static str },
    /// A token-issuing endpoint answered something this flow cannot use.
    #[error("invalid token response: {0}")]
    TokenResponse(String),
    #[error("unreadable JWT: {0}")]
    Jwt(String),
    #[error("chatgpt_account_id missing from token")]
    MissingAccountId,
    #[error("unexpected credential provider: {0:?}")]
    WrongProvider(ProviderId),
    #[error("MCP server \"{server}\" has no refresh token: log in again")]
    MissingRefreshToken { server: String },

    /// The local callback received a request that is not this flow's callback.
    /// The listener answers `404` and keeps waiting.
    #[error("OAuth callback: {0}")]
    Callback(String),
    #[error("OAuth state mismatch (anti-CSRF)")]
    StateMismatch,
    /// The authorization server redirected with `?error=`: the user declined,
    /// or the server refused. Terminal, and the reason is worth showing.
    #[error("authorization refused: {0}")]
    AuthorizationDenied(String),

    #[error("device flow expired (900 s)")]
    DeviceTimeout,
    #[error("device flow denied: {0}")]
    DeviceDenied(String),

    /// A discovered endpoint was not https and not a loopback host. Kept
    /// separate from `Discovery` because it is the refusal that stops an
    /// authorization code from travelling in clear.
    #[error("{what}: refused, {url} is not https (only a loopback host may be plain http)")]
    InsecureEndpoint { what: String, url: String },
    /// Discovery could not produce a usable authorization server, or refused
    /// what it found. The message names the cause.
    #[error("OAuth discovery: {0}")]
    Discovery(String),
    /// A party that has not authenticated yet answered with more bytes than a
    /// document of this kind can legitimately need.
    #[error("response larger than {max} bytes")]
    ResponseTooLarge { max: usize },
}

impl AuthError {
    pub(crate) fn timeout(operation: &'static str) -> Self {
        Self::Timeout { operation }
    }

    pub(crate) fn discovery(reason: impl Into<String>) -> Self {
        Self::Discovery(reason.into())
    }
}
