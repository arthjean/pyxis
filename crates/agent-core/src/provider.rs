//! `Provider` contract + canonical streaming vocabulary.
//!
//! Cargo vs docs reconciliation: `StreamEvent` and the `Provider` trait are
//! conceptually the "provider layer" (PROVIDERS section 2), but **invariant 1**
//! (ARCHITECTURE section 2: `agent-core` does NOT depend on `agent-provider`) requires
//! the **contract** to live here, in the crate of canonical types. `agent-provider`
//! (future) will implement this trait and depend on `agent-core`. The core consumes
//! an injected `dyn Provider`: it knows no concrete adapter.

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::message::{Message, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OpenAiChat,
    /// ChatGPT subscription, Responses API on the ChatGPT backend (ADR-10): the MVP
    /// target. Other providers will be added later (not Ollama: dropped from the
    /// scope, judged too unstable).
    OpenAiChatGpt,
    OpenAiResponses,
    Gemini,
    OpenRouter,
}

/// The only streaming vocabulary the core knows (PROVIDERS section 2). Every
/// adapter must produce THIS sequence. At `ToolCallEnd`, concatenating the
/// `ToolCallDelta.args_json` of a same id MUST yield valid JSON.
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
    /// Encrypted reasoning item (US-031, isolated replay): emitted by the adapter ONLY
    /// when `reasoning_replay` is active. Captured by the `Accumulator`.
    EncryptedReasoning {
        id: String,
        encrypted_content: String,
    },
    /// Subscription quota state read by the adapter (US-003). Purely
    /// informational, emitted at most once per round-trip and only when the
    /// backend serves something usable: an adapter that knows nothing about
    /// quotas never emits it.
    Quota {
        snapshot: crate::quota::QuotaSnapshot,
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

/// Tool definition exposed to the model (input JSON Schema).
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

/// Canonical request (what `ctx.request()` produces). Client-side transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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

/// Non-stream response (utility: titles, compaction summaries).
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
    /// Non-2xx HTTP error. `retry_after_ms` (US-023) carries the parsed server
    /// delay (`Retry-After` / `retry-after-ms`) when present: the loop honors it
    /// through `max(backoff, retry_after)`. `None` = no header -> backoff alone.
    #[error("http {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after_ms: Option<u64>,
    },
    #[error("decode: {0}")]
    Decode(String),
    #[error("stream interrupted: {0}")]
    Stream(String),
    /// CONTEXT error (PTL / 413). It is NOT a transient class: it feeds
    /// withholding (ARCHITECTURE 3.4), not the backoff.
    #[error("context too long (PTL/413)")]
    ContextLengthExceeded,
}

impl ProviderError {
    /// True when the error is a **context** error (PTL/413/input max-tokens)
    /// -> feeds `PendingError`/withholding, never the retry.
    pub fn is_context_error(&self) -> bool {
        matches!(
            self,
            ProviderError::ContextLengthExceeded | ProviderError::Http { status: 413, .. }
        )
    }
}

/// Canonical error taxonomy (ADR-9). Named `ErrorClass` everywhere.
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

/// Implemented by every adapter (in `agent-provider`). Object-safe through
/// `async-trait` -> consumed as `dyn Provider`.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;

    /// Context window to use for a precise slug. Providers without a per-model
    /// table can keep the global capabilities value.
    fn max_context_for_model(&self, model: &str) -> u32 {
        let _ = model;
        self.capabilities().max_context
    }

    /// Context window ACTUALLY declared by the backend for a slug (US-001).
    /// Distinct from `max_context_for_model`, which must always yield a usable
    /// geometry for the compaction thresholds and therefore falls back on a
    /// default. `None` here means "unknown", and a client must then display
    /// nothing rather than a percentage computed on a guessed window.
    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        let _ = model;
        None
    }

    /// Hot path: canonical event stream.
    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;

    /// Non-stream (used by compaction to produce a summary).
    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError>;

    /// Classifies a transport/HTTP error into an `ErrorClass` (source of truth for
    /// the retry). Context errors do NOT go through here (see withholding).
    fn classify_error(&self, err: &ProviderError) -> ErrorClass;

    /// Forced refresh after an expired-auth error reported by the backend.
    /// Providers without OAuth keep the default fatal behavior.
    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        Err(ProviderError::Http {
            status: 401,
            message: "auth refresh unsupported".into(),
            retry_after_ms: None,
        })
    }

    /// Local invalidation of a credential after a user logout. Stateless providers,
    /// or ones without an in-memory credential, can keep the no-op.
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
            reasoning_effort: None,
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
            reasoning_effort: None,
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
            reasoning_effort: None,
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
            reasoning_effort: None,
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
