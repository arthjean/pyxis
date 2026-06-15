//! Contrat `Provider` + vocabulaire de streaming canonique.
//!
//! ⚠️ Réconciliation Cargo vs docs : `StreamEvent` et le trait `Provider` sont
//! conceptuellement « couche provider » (PROVIDERS §2), mais l'**invariant 1**
//! (ARCHITECTURE §2 : `agent-core` ne dépend PAS d'`agent-provider`) impose que
//! le **contrat** vive ici, dans le crate des types canoniques. `agent-provider`
//! (futur) implémentera ce trait et dépendra d'`agent-core`. Le cœur consomme
//! `dyn Provider` injecté — il ne connaît aucun adapter concret.

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::message::{Message, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OpenAiChat,
    /// Abonnement ChatGPT, Responses API sur le backend ChatGPT (ADR-10) — cible
    /// du MVP. Les autres providers s'ajouteront ensuite (pas Ollama : retiré du
    /// scope, jugé trop instable).
    OpenAiChatGpt,
    OpenAiResponses,
    Gemini,
    OpenRouter,
}

/// Le seul vocabulaire de streaming que le cœur connaît (PROVIDERS §2). Tout
/// adapter doit produire CETTE séquence. À `ToolCallEnd`, la concaténation des
/// `ToolCallDelta.args_json` d'un même id DOIT être un JSON valide.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStart { id: ToolCallId, name: String },
    ToolCallDelta { id: ToolCallId, args_json: String },
    ToolCallEnd { id: ToolCallId },
    Usage { usage: TokenUsage },
    Done { stop: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

impl TokenUsage {
    pub fn total(&self) -> u32 {
        self.input.saturating_add(self.output)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub vision: bool,
    pub tools: bool,
    pub prompt_caching: bool,
    pub reasoning: bool,
    pub server_side_state: bool,
    pub max_context: u32,
}

/// Définition d'outil exposée au modèle (JSON Schema d'entrée).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Requête canonique (ce que `ctx.request()` produit). Transcript client-side.
#[derive(Debug, Clone)]
pub struct CanonicalRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
}

/// Réponse non-stream (utilitaire : titres, résumés de compaction).
#[derive(Debug, Clone)]
pub struct CanonicalResponse {
    pub content: Vec<crate::message::ContentBlock>,
    pub usage: TokenUsage,
    pub stop: StopReason,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("http {status}: {message}")]
    Http { status: u16, message: String },
    #[error("décodage: {0}")]
    Decode(String),
    #[error("flux interrompu: {0}")]
    Stream(String),
    /// Erreur de CONTEXTE (PTL / 413). N'est PAS une classe transitoire : elle
    /// alimente le withholding (ARCHITECTURE §3.4), pas le backoff.
    #[error("contexte trop long (PTL/413)")]
    ContextLengthExceeded,
}

impl ProviderError {
    /// Vrai si l'erreur est une erreur de **contexte** (PTL/413/max-tokens
    /// d'entrée) → alimente `PendingError`/withholding, jamais le retry.
    pub fn is_context_error(&self) -> bool {
        matches!(
            self,
            ProviderError::ContextLengthExceeded | ProviderError::Http { status: 413, .. }
        )
    }
}

/// Taxonomie d'erreurs canonique (ADR-9). Nommée `ErrorClass` partout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Retryable,
    RateLimited,
    Overloaded(u16),
    Auth(AuthError),
    InvalidRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Expired,
    ThirdPartyBlocked,
    Invalid,
}

/// Implémenté par chaque adapter (dans `agent-provider`). Object-safe via
/// `async-trait` → consommé en `dyn Provider`.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;

    /// Chemin chaud : flux d'événements canoniques.
    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;

    /// Non-stream (utilisé par la compaction pour produire un résumé).
    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError>;

    /// Classifie une erreur transport/HTTP en `ErrorClass` (source de vérité du
    /// retry). Les erreurs de contexte ne passent PAS par ici (cf. withholding).
    fn classify_error(&self, err: &ProviderError) -> ErrorClass;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_error_detection() {
        assert!(ProviderError::ContextLengthExceeded.is_context_error());
        assert!(
            ProviderError::Http {
                status: 413,
                message: "too long".into()
            }
            .is_context_error()
        );
        assert!(
            !ProviderError::Http {
                status: 529,
                message: "overloaded".into()
            }
            .is_context_error()
        );
        assert!(!ProviderError::Transport("reset".into()).is_context_error());
    }
}
