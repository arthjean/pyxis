//! High-level core error, propagated to clients through `AgentEvent::Error`.

use crate::provider::{AuthError, ErrorClass, ProviderError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    Transport,
    Http,
    Decode,
    Stream,
    Credential,
    ContextLengthExceeded,
    Contract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailure {
    pub kind: ProviderFailureKind,
    pub status: Option<u16>,
    pub message: String,
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ErrorClass>,
}

impl ProviderFailure {
    pub fn contract(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderFailureKind::Contract,
            status: None,
            message: message.into(),
            retry_after_ms: None,
            class: None,
        }
    }

    pub fn classified(error: &ProviderError, class: ErrorClass) -> Self {
        Self {
            class: Some(class),
            ..Self::from(error)
        }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "http {status}: {}", self.message),
            None => write!(
                f,
                "{}: {}",
                format!("{:?}", self.kind).to_lowercase(),
                self.message
            ),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentError {
    #[error("provider: {0}")]
    Provider(ProviderFailure),
    #[error("auth: {0:?}")]
    Auth(AuthError),
    #[error("unrecoverable context (compaction failed): {0}")]
    ContextUnrecoverable(String),
    #[error("compaction: circuit breaker ({0} consecutive failures)")]
    CompactionCircuitBreaker(u32),
    #[error("compaction: {0}")]
    Compaction(String),
    #[error("session: {0}")]
    Session(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl From<&ProviderError> for AgentError {
    fn from(e: &ProviderError) -> Self {
        AgentError::Provider(ProviderFailure::from(e))
    }
}

impl From<ProviderError> for AgentError {
    fn from(e: ProviderError) -> Self {
        AgentError::Provider(ProviderFailure::from(&e))
    }
}

impl From<&ProviderError> for ProviderFailure {
    fn from(e: &ProviderError) -> Self {
        match e {
            ProviderError::Transport(_) => Self {
                kind: ProviderFailureKind::Transport,
                status: None,
                message: "provider transport failed".to_string(),
                retry_after_ms: None,
                class: None,
            },
            ProviderError::Http {
                status,
                retry_after_ms,
                ..
            } => Self {
                kind: ProviderFailureKind::Http,
                status: Some(*status),
                message: "provider HTTP request failed".to_string(),
                retry_after_ms: *retry_after_ms,
                class: None,
            },
            ProviderError::Decode(_) => Self {
                kind: ProviderFailureKind::Decode,
                status: None,
                message: "provider response decode failed".to_string(),
                retry_after_ms: None,
                class: None,
            },
            ProviderError::Stream(_) => Self {
                kind: ProviderFailureKind::Stream,
                status: None,
                message: "provider stream failed".to_string(),
                retry_after_ms: None,
                class: None,
            },
            ProviderError::Credential(_) => Self {
                kind: ProviderFailureKind::Credential,
                status: None,
                message: "credential recovery required".to_string(),
                retry_after_ms: None,
                class: None,
            },
            ProviderError::ContextLengthExceeded => Self {
                kind: ProviderFailureKind::ContextLengthExceeded,
                status: Some(413),
                message: "context too long (PTL/413)".to_string(),
                retry_after_ms: None,
                class: None,
            },
        }
    }
}
