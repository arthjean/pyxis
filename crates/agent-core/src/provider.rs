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

use crate::message::{ToolCallFormat, ToolCallId};
use crate::model::{ModelRuntimeError, ResolvedModelRuntime};

pub use crate::provider_extension::{MAX_PROVIDER_EXTENSION_BYTES, ProviderExtension};
pub use crate::request::{
    CanonicalRequest, CanonicalRequestValidationError, OutputSchema, ReasoningSummaryDelivery,
    RequestStreamOptions,
};
pub use crate::response_metadata::{ReasoningMetadata, ResponseMetadata, SafetyMetadata};

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
/// adapter must produce THIS sequence. Deltas are observable fragments; when a
/// provider later sends an authoritative complete input, `ToolCallInputDone`
/// replaces the accumulated fragments before `ToolCallEnd`.
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
        /// Fixed at the start: a call cannot change format mid-stream, and the
        /// accumulator refuses a delta that contradicts it.
        #[serde(default)]
        format: ToolCallFormat,
    },
    ToolCallDelta {
        id: ToolCallId,
        input_delta: String,
    },
    /// Complete input from the authoritative terminal item. Deltas stay
    /// observable at arrival time; the accumulator replaces them here rather
    /// than receiving a duplicate terminal delta.
    ToolCallInputDone {
        id: ToolCallId,
        input: String,
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
    /// The backend rejected replay once, so the adapter retried the same
    /// sampling without it and the core disables replay for the rest of the turn.
    ReasoningReplayDisabled {
        reason: String,
    },
    /// Subscription quota state read by the adapter (US-003). Purely
    /// informational, emitted at most once per round-trip and only when the
    /// backend serves something usable: an adapter that knows nothing about
    /// quotas never emits it.
    Quota {
        snapshot: crate::quota::QuotaSnapshot,
    },
    /// Additive response and transport metadata. Every field is optional so an
    /// adapter can publish values as soon as they become available without
    /// fabricating a complete response envelope.
    ResponseMetadata {
        metadata: Box<ResponseMetadata>,
    },
    /// An additive provider event that has no canonical variant yet. Its
    /// payload is sanitized and bounded before it crosses the provider seam.
    ProviderExtension {
        extension: ProviderExtension,
    },
    /// The backend produced a response item this adapter does not map. The
    /// content is genuinely lost, so the LOSS is reported instead of being
    /// swallowed: Codex keeps such an item as `ResponseItem::Other`
    /// (`codex-rs/protocol/src/models.rs:1041`), which is how a newly served
    /// item type stays visible rather than vanishing from the stream.
    ///
    /// `item_type` keeps the wire tag while `extension`, when present, carries a
    /// bounded and sanitized copy of the item for forward-compatible clients.
    UnmappedItem {
        item_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<ProviderExtension>,
    },
}

impl StreamEvent {
    /// Start of a call whose arguments are JSON.
    pub fn tool_call_start(id: impl Into<ToolCallId>, name: impl Into<String>) -> Self {
        Self::ToolCallStart {
            id: id.into(),
            name: name.into(),
            format: ToolCallFormat::Json,
        }
    }

    /// Start of a freeform call whose input is text.
    pub fn custom_tool_call_start(id: impl Into<ToolCallId>, name: impl Into<String>) -> Self {
        Self::ToolCallStart {
            id: id.into(),
            name: name.into(),
            format: ToolCallFormat::Text,
        }
    }
}

/// Token counts of one model round-trip.
///
/// `input` INCLUDES the cached prefix: it is the size of the context actually
/// submitted, which is what the compaction threshold reads (ARCHITECTURE 3.3).
/// The breakdown fields answer a different question, cost and cache efficiency,
/// and never feed the budget. Every one of them defaults to zero so a provider
/// that reports nothing but the two totals stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    /// Share of `input` served from the backend prefix cache.
    #[serde(default)]
    pub cached_input: u64,
    /// Share of `input` written to the cache by this round-trip (billed apart
    /// on the backends that meter it).
    #[serde(default)]
    pub cache_write_input: u64,
    pub output: u64,
    /// Share of `output` spent on reasoning rather than on visible text.
    #[serde(default)]
    pub reasoning_output: u64,
    /// Total as the backend reports it. Zero means "not reported", which is why
    /// [`TokenUsage::total`] falls back on the sum rather than trusting it blind.
    #[serde(default)]
    pub total: u64,
}

impl TokenUsage {
    /// Counts of a provider that reports nothing but the two totals.
    pub fn new(input: u64, output: u64) -> Self {
        Self {
            input,
            output,
            ..Self::default()
        }
    }

    /// Backend total when it serves one, the local sum otherwise. The two can
    /// differ: a backend may count tokens we never see.
    pub fn total(&self) -> u64 {
        match self.total {
            0 => self.input.saturating_add(self.output),
            reported => reported,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    /// Input actually paid for at full price.
    pub fn non_cached_input(&self) -> u64 {
        self.input.saturating_sub(self.cached_input)
    }

    /// Single absolute value worth displaying: what this round-trip cost beyond
    /// the cache. Ported from Codex `TokenUsage::blended_total`
    /// (`codex-rs/protocol/src/protocol.rs:2231`).
    pub fn blended_total(&self) -> u64 {
        self.non_cached_input().saturating_add(self.output)
    }

    /// Element-wise accumulation, so a run total is the sum of its round-trips.
    pub fn add_assign(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.cached_input = self.cached_input.saturating_add(other.cached_input);
        self.cache_write_input = self
            .cache_write_input
            .saturating_add(other.cache_write_input);
        self.output = self.output.saturating_add(other.output);
        self.reasoning_output = self.reasoning_output.saturating_add(other.reasoning_output);
        // `total()` and not `total`: a backend that reports no total must not
        // make the accumulated one collapse to zero.
        self.total = self.total().saturating_add(other.total());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    Continue,
    ToolUse,
    MaxTokens,
    ContentFilter,
    IncompleteUnknown,
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

impl Capabilities {
    /// Refuses a tool plan this provider cannot serialize, BEFORE any network
    /// call. A freeform tool projected onto a function wire would either lose
    /// its grammar or gain a fabricated schema; both are silent corruption, so
    /// the incompatibility is typed and local.
    pub fn ensure_tools_supported(&self, tools: &[ToolSpec]) -> Result<(), ProviderError> {
        let Some(first) = tools.first() else {
            return Ok(());
        };
        if !self.tools {
            return Err(ProviderError::UnsupportedTool {
                tool: first.name.clone(),
                reason: "provider does not support tool calling".into(),
            });
        }
        for tool in tools {
            if tool.is_freeform() && !self.tool_calling.freeform_tools {
                return Err(ProviderError::UnsupportedTool {
                    tool: tool.name.clone(),
                    reason: "provider does not support freeform tools".into(),
                });
            }
        }
        Ok(())
    }
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
    /// The adapter can serialize a freeform tool. Default `false`: a provider
    /// that says nothing refuses the plan instead of silently degrading a
    /// freeform tool into a function one.
    #[serde(default)]
    pub freeform_tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    pub encrypted_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCapabilities {
    pub prompt_cache_key: bool,
}

/// Grammar syntax a freeform tool constrains its text input with. Kept as an
/// enum, not a free string: an unknown syntax must fail to build rather than
/// reach a backend that would reject the whole request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarSyntax {
    Lark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGrammar {
    pub syntax: GrammarSyntax,
    pub definition: String,
}

/// How a tool receives its input. This is the provider-neutral algebra: a
/// function takes JSON validated by a schema, a freeform tool takes text with
/// an optional grammar. A freeform tool carries NO `input_schema`, so no
/// adapter can invent one to fit a function-shaped wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolKind {
    Function {
        input_schema: serde_json::Value,
    },
    Freeform {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grammar: Option<ToolGrammar>,
    },
}

/// Tool definition exposed to the model.
///
/// `PartialEq` is what lets US-006 decide that a step frame did not move: a
/// catalog compared equal keeps its generation, hence its bytes and its cache
/// prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(flatten)]
    pub kind: ToolKind,
}

impl ToolSpec {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: ToolKind::Function { input_schema },
        }
    }

    pub fn freeform(
        name: impl Into<String>,
        description: impl Into<String>,
        grammar: Option<ToolGrammar>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: ToolKind::Freeform { grammar },
        }
    }

    /// `None` for a freeform tool: callers that need a schema must handle its
    /// absence instead of receiving an empty object that looks like one.
    pub fn input_schema(&self) -> Option<&serde_json::Value> {
        match &self.kind {
            ToolKind::Function { input_schema } => Some(input_schema),
            ToolKind::Freeform { .. } => None,
        }
    }

    pub fn is_freeform(&self) -> bool {
        matches!(self.kind, ToolKind::Freeform { .. })
    }

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
        match &self.kind {
            ToolKind::Function { input_schema } => {
                let Some(schema) = input_schema.as_object() else {
                    return Err(ToolSpecValidationError::SchemaMustBeObject {
                        tool: self.name.clone(),
                    });
                };
                if !schema_has_object_type(schema) {
                    return Err(ToolSpecValidationError::SchemaMustBeObject {
                        tool: self.name.clone(),
                    });
                }
                validate_strict_schema_object(&self.name, input_schema)?;
            }
            ToolKind::Freeform { grammar } => {
                if let Some(grammar) = grammar
                    && grammar.definition.trim().is_empty()
                {
                    return Err(ToolSpecValidationError::EmptyGrammarDefinition {
                        tool: self.name.clone(),
                    });
                }
            }
        }
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
    #[error("tool {tool} declares a grammar with an empty definition")]
    EmptyGrammarDefinition { tool: String },
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
    /// Typed provider/API failure with the diagnostic identifiers that are safe
    /// and useful outside the adapter. Credential and account values never enter
    /// this payload.
    #[error("provider {category:?}: {message}")]
    Api {
        category: ProviderErrorCategory,
        status: Option<u16>,
        message: String,
        retry_after_ms: Option<u64>,
        request_id: Option<String>,
        auth_request_id: Option<String>,
    },
    #[error("decode: {0}")]
    Decode(String),
    #[error("stream interrupted: {0}")]
    Stream(String),
    /// Credential preparation or recovery already failed before a provider
    /// request could be opened. The payload is typed and contains no backend
    /// body or credential detail.
    #[error("credential: {0:?}")]
    Credential(AuthError),
    /// The plan contains a tool this provider cannot represent on its wire.
    /// Raised before opening a request: no attempt, no backoff, no retry.
    #[error("tool {tool} is unsupported: {reason}")]
    UnsupportedTool { tool: String, reason: String },
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
            ProviderError::ContextLengthExceeded
                | ProviderError::Http { status: 413, .. }
                | ProviderError::Api {
                    category: ProviderErrorCategory::ContextOverflow,
                    ..
                }
        )
    }
}

/// Stable provider-neutral categories used by HTTP and streamed failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategory {
    ContextOverflow,
    Quota,
    UsageNotIncluded,
    CyberPolicy,
    InvalidPrompt,
    InvalidImage,
    RateLimited,
    Overloaded,
    Authentication,
    PermissionDenied,
    Incomplete,
    InvalidRequest,
    Failed,
}

/// Canonical error taxonomy (ADR-9). Named `ErrorClass` everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Retryable,
    RateLimited,
    Overloaded(u16),
    /// A context-limit failure may reopen only through the same total attempt
    /// budget, after reactive compaction.
    ContextLimit,
    /// The request is valid without encrypted reasoning replay. The core owns
    /// this reopening so it consumes the same sampling attempt budget.
    ReasoningReplayRejected,
    Auth(AuthError),
    InvalidRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthError {
    Expired,
    ThirdPartyBlocked,
    Invalid,
    /// Recovery is unavailable, rejected, or already consumed for this sampling.
    ReconnectRequired,
}

/// Implemented by every adapter (in `agent-provider`). Object-safe through
/// `async-trait` -> consumed as `dyn Provider`.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;

    /// Tool mode of `slug`, for the surfaces that must compose a tool plan
    /// WITHOUT resolving a full runtime (a step boundary is one of them).
    /// `Direct` by default, which is what an adapter that knows no code mode
    /// should answer.
    fn tool_mode(&self, _slug: &str) -> crate::model::ModelToolMode {
        crate::model::ModelToolMode::Direct
    }

    /// Multi-agent protocol of `slug`, read at the same step boundary as
    /// [`Provider::tool_mode`]. `Disabled` by default: an adapter that knows
    /// nothing about orchestration must not hand a model orchestration tools.
    fn multi_agent_version(&self, _slug: &str) -> crate::model::MultiAgentVersion {
        crate::model::MultiAgentVersion::Disabled
    }

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

    /// Resolves the immutable model contract before a turn is started.
    fn resolve_model_runtime(
        &self,
        model: &str,
        reasoning_effort: Option<&str>,
        max_output_tokens: u32,
        max_retries: u32,
        backoff_base_ms: u64,
    ) -> Result<ResolvedModelRuntime, ModelRuntimeError> {
        let _ = (
            reasoning_effort,
            max_output_tokens,
            max_retries,
            backoff_base_ms,
        );
        Err(ModelRuntimeError::Incompatible {
            slug: model.to_string(),
            reason: "provider does not expose a model runtime resolver".into(),
        })
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
    fn strict_tool_schema_requires_all_properties() {
        let spec = ToolSpec::function(
            "read",
            "lit",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": ["integer", "null"] }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        );
        assert!(matches!(
            spec.validate(),
            Err(ToolSpecValidationError::RequiredMustMatchProperties { tool }) if tool == "read"
        ));
    }

    #[test]
    fn strict_tool_schema_accepts_nullable_required_optionals() {
        let spec = ToolSpec::function(
            "read",
            "lit",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": ["integer", "null"] }
                },
                "required": ["path", "offset"],
                "additionalProperties": false
            }),
        );
        spec.validate().unwrap();
    }

    fn exec_grammar() -> ToolGrammar {
        ToolGrammar {
            syntax: GrammarSyntax::Lark,
            definition: "start: SOURCE\nSOURCE: /[\\s\\S]+/".into(),
        }
    }

    #[test]
    fn freeform_tool_carries_no_schema_and_survives_a_round_trip() {
        let spec = ToolSpec::freeform("exec", "run javascript", Some(exec_grammar()));
        spec.validate().unwrap();
        assert!(spec.is_freeform());
        assert!(
            spec.input_schema().is_none(),
            "a freeform tool must not expose a fabricated schema"
        );

        let encoded = serde_json::to_value(&spec).unwrap();
        assert_eq!(encoded["kind"], "freeform");
        assert!(encoded.get("input_schema").is_none());
        assert_eq!(encoded["grammar"]["syntax"], "lark");
        let decoded: ToolSpec = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, spec);

        // A text-only freeform tool keeps no grammar at all.
        let plain = ToolSpec::freeform("notes", "free text", None);
        plain.validate().unwrap();
        let encoded = serde_json::to_value(&plain).unwrap();
        assert!(encoded.get("grammar").is_none());
        assert_eq!(
            serde_json::from_value::<ToolSpec>(encoded).unwrap(),
            plain,
            "an absent grammar round-trips as absent"
        );
    }

    #[test]
    fn function_tool_keeps_its_name_description_and_schema_through_the_algebra() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        });
        let spec = ToolSpec::function("read", "reads a file", schema.clone());
        spec.validate().unwrap();
        assert_eq!(spec.name, "read");
        assert_eq!(spec.description, "reads a file");
        assert_eq!(spec.input_schema(), Some(&schema));
        assert!(!spec.is_freeform());
        let decoded: ToolSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(decoded, spec);
    }

    #[test]
    fn empty_grammar_definition_is_rejected_before_exposure() {
        let spec = ToolSpec::freeform(
            "exec",
            "run",
            Some(ToolGrammar {
                syntax: GrammarSyntax::Lark,
                definition: "   ".into(),
            }),
        );
        assert!(matches!(
            spec.validate(),
            Err(ToolSpecValidationError::EmptyGrammarDefinition { tool }) if tool == "exec"
        ));
    }

    #[test]
    fn provider_without_freeform_support_refuses_the_plan_before_any_call() {
        let mut capabilities = Capabilities {
            tools: true,
            ..Capabilities::default()
        };
        let plan = vec![
            ToolSpec::function(
                "read",
                "reads",
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            ),
            ToolSpec::freeform("exec", "run javascript", Some(exec_grammar())),
        ];
        let error = capabilities
            .ensure_tools_supported(&plan)
            .expect_err("freeform is not supported by default");
        assert!(matches!(
            &error,
            ProviderError::UnsupportedTool { tool, .. } if tool == "exec"
        ));
        assert!(error.to_string().contains("freeform"));

        capabilities.tool_calling.freeform_tools = true;
        capabilities.ensure_tools_supported(&plan).unwrap();

        capabilities.tools = false;
        assert!(matches!(
            capabilities.ensure_tools_supported(&plan),
            Err(ProviderError::UnsupportedTool { .. })
        ));
        // No tool at all stays valid on a provider without tool support.
        capabilities.ensure_tools_supported(&[]).unwrap();
    }
}
