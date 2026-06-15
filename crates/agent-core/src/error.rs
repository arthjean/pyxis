//! Erreur de haut niveau du cœur, propagée aux clients via `AgentEvent::Error`.

use crate::provider::{AuthError, ProviderError};

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error("provider: {0}")]
    Provider(String),
    #[error("auth: {0:?}")]
    Auth(AuthError),
    #[error("contexte irrécupérable (compaction échouée): {0}")]
    ContextUnrecoverable(String),
    #[error("compaction: circuit breaker ({0} échecs consécutifs)")]
    CompactionCircuitBreaker(u32),
    #[error("compaction: {0}")]
    Compaction(String),
    #[error("session: {0}")]
    Session(String),
    #[error("requête invalide: {0}")]
    InvalidRequest(String),
}

impl From<&ProviderError> for AgentError {
    fn from(e: &ProviderError) -> Self {
        AgentError::Provider(e.to_string())
    }
}

impl From<ProviderError> for AgentError {
    fn from(e: ProviderError) -> Self {
        AgentError::Provider(e.to_string())
    }
}
