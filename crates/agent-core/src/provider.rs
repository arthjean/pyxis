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
use std::collections::HashSet;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStart {
        id: ToolCallId,
        name: String,
    },
    ToolCallDelta {
        id: ToolCallId,
        args_json: String,
    },
    ToolCallEnd {
        id: ToolCallId,
    },
    Usage {
        usage: TokenUsage,
    },
    Done {
        stop: StopReason,
    },
    /// Reasoning item chiffré (US-031, replay isolé) : émis par l'adapter UNIQUEMENT
    /// si `reasoning_replay` est actif. Capturé par l'`Accumulator`.
    EncryptedReasoning {
        id: String,
        encrypted_content: String,
    },
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub vision: bool,
    pub tools: bool,
    pub prompt_caching: bool,
    pub reasoning: bool,
    pub server_side_state: bool,
    pub max_context: u32,
    #[serde(default)]
    pub limits: CapabilityLimits,
    #[serde(default)]
    pub tool_calling: ToolCallingCapabilities,
    #[serde(default)]
    pub reasoning_options: ReasoningCapabilities,
    #[serde(default)]
    pub cache: CacheCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilityLimits {
    pub max_images_per_request: Option<u32>,
    pub max_tool_schema_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCallingCapabilities {
    pub parallel_tool_calls: bool,
    pub strict_json_schema: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    pub encrypted_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCapabilities {
    pub prompt_cache_key: bool,
}

/// Définition d'outil exposée au modèle (JSON Schema d'entrée).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl ToolSpec {
    pub fn validate(&self) -> Result<(), ToolSpecValidationError> {
        if self.name.trim().is_empty() {
            return Err(ToolSpecValidationError::EmptyName);
        }
        if self.name.len() > 64
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ToolSpecValidationError::InvalidName {
                tool: self.name.clone(),
            });
        }
        let Some(schema) = self.input_schema.as_object() else {
            return Err(ToolSpecValidationError::SchemaMustBeObject {
                tool: self.name.clone(),
            });
        };
        if !schema_has_object_type(schema) {
            return Err(ToolSpecValidationError::SchemaMustBeObject {
                tool: self.name.clone(),
            });
        }
        validate_strict_schema_object(&self.name, &self.input_schema)?;
        Ok(())
    }
}

fn schema_has_object_type(schema: &serde_json::Map<String, serde_json::Value>) -> bool {
    match schema.get("type") {
        Some(serde_json::Value::String(kind)) => kind == "object",
        Some(serde_json::Value::Array(kinds)) => {
            kinds.iter().any(|kind| kind.as_str() == Some("object"))
        }
        _ => false,
    }
}

fn validate_strict_schema_object(
    tool: &str,
    schema: &serde_json::Value,
) -> Result<(), ToolSpecValidationError> {
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };

    if schema_has_object_type(obj) {
        if obj.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
            return Err(
                ToolSpecValidationError::SchemaMustDenyAdditionalProperties {
                    tool: tool.to_string(),
                },
            );
        }
        let property_names: HashSet<String> = match obj.get("properties") {
            None => HashSet::new(),
            Some(serde_json::Value::Object(props)) => props.keys().cloned().collect(),
            Some(_) => {
                return Err(ToolSpecValidationError::SchemaPropertiesMustBeObject {
                    tool: tool.to_string(),
                });
            }
        };
        let required_names = required_names(tool, obj)?;
        if required_names != property_names {
            return Err(ToolSpecValidationError::RequiredMustMatchProperties {
                tool: tool.to_string(),
            });
        }
    }

    if let Some(serde_json::Value::Object(props)) = obj.get("properties") {
        for schema in props.values() {
            validate_strict_schema_object(tool, schema)?;
        }
    }
    for key in ["items", "additionalItems", "contains"] {
        if let Some(schema) = obj.get(key) {
            validate_strict_schema_object(tool, schema)?;
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(serde_json::Value::Array(items)) = obj.get(key) {
            for schema in items {
                validate_strict_schema_object(tool, schema)?;
            }
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(serde_json::Value::Object(defs)) = obj.get(key) {
            for schema in defs.values() {
                validate_strict_schema_object(tool, schema)?;
            }
        }
    }
    Ok(())
}

fn required_names(
    tool: &str,
    schema: &serde_json::Map<String, serde_json::Value>,
) -> Result<HashSet<String>, ToolSpecValidationError> {
    match schema.get("required") {
        None => Ok(HashSet::new()),
        Some(serde_json::Value::Array(items)) => {
            let mut names = HashSet::new();
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(ToolSpecValidationError::SchemaRequiredMustBeStringArray {
                        tool: tool.to_string(),
                    });
                };
                names.insert(name.to_string());
            }
            Ok(names)
        }
        Some(_) => Err(ToolSpecValidationError::SchemaRequiredMustBeStringArray {
            tool: tool.to_string(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolSpecValidationError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("tool {tool} name must be <=64 chars and use only ASCII letters, digits, _ or -")]
    InvalidName { tool: String },
    #[error("tool {tool} input_schema must be a JSON schema object")]
    SchemaMustBeObject { tool: String },
    #[error("tool {tool} input_schema must set additionalProperties=false")]
    SchemaMustDenyAdditionalProperties { tool: String },
    #[error("tool {tool} input_schema properties must be an object")]
    SchemaPropertiesMustBeObject { tool: String },
    #[error("tool {tool} input_schema required must be an array of strings")]
    SchemaRequiredMustBeStringArray { tool: String },
    #[error("tool {tool} required fields must include every property for strict schema mode")]
    RequiredMustMatchProperties { tool: String },
}

/// Requête canonique (ce que `ctx.request()` produit). Transcript client-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
}

impl CanonicalRequest {
    pub fn validate(&self) -> Result<(), CanonicalRequestValidationError> {
        if self.model.trim().is_empty() {
            return Err(CanonicalRequestValidationError::EmptyModel);
        }
        if self.max_output_tokens == 0 {
            return Err(CanonicalRequestValidationError::ZeroMaxOutputTokens);
        }
        for (index, message) in self.messages.iter().enumerate() {
            message.validate().map_err(|source| {
                CanonicalRequestValidationError::InvalidMessage {
                    index,
                    detail: source.to_string(),
                }
            })?;
        }
        for tool in &self.tools {
            tool.validate()
                .map_err(CanonicalRequestValidationError::InvalidTool)?;
        }
        let mut seen_tools = HashSet::new();
        for tool in &self.tools {
            if !seen_tools.insert(tool.name.as_str()) {
                return Err(CanonicalRequestValidationError::DuplicateToolName {
                    tool: tool.name.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalRequestValidationError {
    #[error("model is empty")]
    EmptyModel,
    #[error("max_output_tokens must be greater than zero")]
    ZeroMaxOutputTokens,
    #[error("message {index} is invalid: {detail}")]
    InvalidMessage { index: usize, detail: String },
    #[error("tool spec is invalid: {0}")]
    InvalidTool(#[from] ToolSpecValidationError),
    #[error("duplicate tool name: {tool}")]
    DuplicateToolName { tool: String },
}

/// Réponse non-stream (utilitaire : titres, résumés de compaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub content: Vec<crate::message::ContentBlock>,
    pub usage: TokenUsage,
    pub stop: StopReason,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("transport: {0}")]
    Transport(String),
    /// Erreur HTTP non-2xx. `retry_after_ms` (US-023) porte le délai serveur
    /// parsé (`Retry-After` / `retry-after-ms`) quand présent : la boucle l'honore
    /// via `max(backoff, retry_after)`. `None` = pas d'en-tête → backoff seul.
    #[error("http {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after_ms: Option<u64>,
    },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Retryable,
    RateLimited,
    Overloaded(u16),
    Auth(AuthError),
    InvalidRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// Fenêtre de contexte à utiliser pour un slug précis. Les providers sans
    /// table par modèle peuvent conserver la valeur globale des capabilities.
    fn max_context_for_model(&self, model: &str) -> u32 {
        let _ = model;
        self.capabilities().max_context
    }

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

    /// Refresh forcé après une erreur d'auth expirée remontée par le backend.
    /// Les providers sans OAuth gardent le comportement fatal par défaut.
    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        Err(ProviderError::Http {
            status: 401,
            message: "auth refresh unsupported".into(),
            retry_after_ms: None,
        })
    }

    /// Invalidation locale d'une credential après logout utilisateur. Les providers
    /// stateless ou sans credential en mémoire peuvent garder le no-op.
    async fn disconnect_auth(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn set_prompt_cache_key(&self, _key: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Message, Role};

    #[test]
    fn context_error_detection() {
        assert!(ProviderError::ContextLengthExceeded.is_context_error());
        assert!(
            ProviderError::Http {
                status: 413,
                message: "too long".into(),
                retry_after_ms: None,
            }
            .is_context_error()
        );
        assert!(
            !ProviderError::Http {
                status: 529,
                message: "overloaded".into(),
                retry_after_ms: None,
            }
            .is_context_error()
        );
        assert!(!ProviderError::Transport("reset".into()).is_context_error());
    }

    #[test]
    fn canonical_request_validation_rejects_invalid_message_and_tool_schema() {
        let invalid_message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "out".into(),
                untrusted: true,
                is_error: false,
                error_kind: None,
            }],
        };
        let req = CanonicalRequest {
            model: "gpt".into(),
            system: None,
            messages: vec![invalid_message],
            tools: vec![],
            max_output_tokens: 100,
        };
        assert!(matches!(
            req.validate(),
            Err(CanonicalRequestValidationError::InvalidMessage { .. })
        ));

        let req = CanonicalRequest {
            model: "gpt".into(),
            system: None,
            messages: vec![Message::user("ok")],
            tools: vec![ToolSpec {
                name: "bad".into(),
                description: String::new(),
                input_schema: serde_json::json!({
                    "type": "string",
                    "additionalProperties": false,
                    "required": []
                }),
            }],
            max_output_tokens: 100,
        };
        assert!(matches!(
            req.validate(),
            Err(CanonicalRequestValidationError::InvalidTool(
                ToolSpecValidationError::SchemaMustBeObject { .. }
            ))
        ));
    }

    #[test]
    fn canonical_request_rejects_non_strict_tool_schemas_and_duplicate_names() {
        let req = CanonicalRequest {
            model: "gpt".into(),
            system: None,
            messages: vec![Message::user("ok")],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "lit".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            }],
            max_output_tokens: 100,
        };
        assert!(matches!(
            req.validate(),
            Err(CanonicalRequestValidationError::InvalidTool(
                ToolSpecValidationError::SchemaMustDenyAdditionalProperties { .. }
            ))
        ));

        let strict_tool = ToolSpec {
            name: "read".into(),
            description: "lit".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        };
        let req = CanonicalRequest {
            model: "gpt".into(),
            system: None,
            messages: vec![Message::user("ok")],
            tools: vec![strict_tool.clone(), strict_tool],
            max_output_tokens: 100,
        };
        assert!(matches!(
            req.validate(),
            Err(CanonicalRequestValidationError::DuplicateToolName { tool }) if tool == "read"
        ));
    }

    #[test]
    fn strict_tool_schema_requires_all_properties() {
        let spec = ToolSpec {
            name: "read".into(),
            description: "lit".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": ["integer", "null"] }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        };
        assert!(matches!(
            spec.validate(),
            Err(ToolSpecValidationError::RequiredMustMatchProperties { tool }) if tool == "read"
        ));
    }

    #[test]
    fn strict_tool_schema_accepts_nullable_required_optionals() {
        let spec = ToolSpec {
            name: "read".into(),
            description: "lit".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": ["integer", "null"] }
                },
                "required": ["path", "offset"],
                "additionalProperties": false
            }),
        };
        spec.validate().unwrap();
    }
}
