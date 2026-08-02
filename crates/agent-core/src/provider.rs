//! `Provider` contract + canonical streaming vocabulary.
//!
//! Cargo vs docs reconciliation: `StreamEvent` and the `Provider` trait are
//! conceptually the "provider layer" (PROVIDERS section 2), but **invariant 1**
//! (ARCHITECTURE section 2: `agent-core` does NOT depend on `agent-provider`) requires
//! the **contract** to live here, in the crate of canonical types. `agent-provider`
//! (future) will implement this trait and depend on `agent-core`. The core consumes
//! an injected `dyn Provider`: it knows no concrete adapter.

use futures_util::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};

use crate::message::{ToolCallFormat, ToolCallId};
use crate::model::{ModelRuntimeError, ResolvedModelRuntime};

pub use crate::provider_extension::{MAX_PROVIDER_EXTENSION_BYTES, ProviderExtension};
pub use crate::request::{
    CanonicalRequest, CanonicalRequestValidationError, OutputSchema, ReasoningSummaryDelivery,
    RequestStreamOptions, TURN_ID_METADATA_KEY,
};
pub use crate::response_item::{
    ResponseItem, ResponseItemError, ResponseItemKind, ResponseItemPhase,
};
pub use crate::response_metadata::{ReasoningMetadata, ResponseMetadata, SafetyMetadata};
pub use crate::tool_spec::{
    GrammarSyntax, ToolGrammar, ToolKind, ToolSpec, ToolSpecValidationError, WebSearchContextSize,
    WebSearchFilters, WebSearchLocation,
};

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
    AmazonBedrock,
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
    /// Complete bounded Responses item at its wire lifecycle boundary. Added
    /// and done remain separate; the done payload is authoritative and never
    /// replayed as content deltas.
    ResponseItem {
        phase: ResponseItemPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_index: Option<u64>,
        item: Box<ResponseItem>,
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
    #[serde(default)]
    pub structured_output: bool,
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
    /// Validates invariants shared by every provider configuration. Wire-specific
    /// restrictions remain the responsibility of the adapter.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let invalid = |capability: &str, reason: &str| ProviderError::UnsupportedCapability {
            capability: capability.into(),
            reason: reason.into(),
        };
        if self.max_context == 0 {
            return Err(invalid("max_context", "must be nonzero"));
        }
        if !self.tools
            && (self.tool_calling.parallel_tool_calls
                || self.tool_calling.strict_json_schema
                || self.tool_calling.freeform_tools
                || self.tool_calling.namespace_tools
                || self.tool_calling.tool_search
                || self.tool_calling.web_search)
        {
            return Err(invalid("tools", "tool modes require tool calling"));
        }
        if !self.reasoning && self.reasoning_options.encrypted_replay {
            return Err(invalid(
                "reasoning",
                "encrypted replay requires reasoning support",
            ));
        }
        if !self.prompt_caching && self.cache.prompt_cache_key {
            return Err(invalid(
                "prompt_caching",
                "prompt cache keys require prompt caching",
            ));
        }
        Ok(())
    }

    /// Refuses canonical request features absent from the provider declaration
    /// before credential resolution or network access.
    pub fn ensure_request_supported(
        &self,
        request: &CanonicalRequest,
    ) -> Result<(), ProviderError> {
        let unsupported = |capability: &str, reason: &str| ProviderError::UnsupportedCapability {
            capability: capability.into(),
            reason: reason.into(),
        };
        if request.output_schema.is_some() && !self.structured_output {
            return Err(unsupported(
                "structured_output",
                "provider does not support structured output",
            ));
        }
        if (request.reasoning_effort.is_some() || request.reasoning_replay) && !self.reasoning {
            return Err(unsupported(
                "reasoning",
                "provider does not support reasoning controls",
            ));
        }
        if request.cache_key.is_some() && (!self.prompt_caching || !self.cache.prompt_cache_key) {
            return Err(unsupported(
                "prompt_cache_key",
                "provider does not support prompt cache keys",
            ));
        }
        if request.messages.iter().any(|message| message.has_images()) && !self.vision {
            return Err(unsupported(
                "vision",
                "provider does not support image input",
            ));
        }
        self.ensure_tools_supported(&request.tools)
    }

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
            self.ensure_tool_supported(tool)?;
        }
        Ok(())
    }

    fn ensure_tool_supported(&self, tool: &ToolSpec) -> Result<(), ProviderError> {
        let unsupported = |reason: &str| ProviderError::UnsupportedTool {
            tool: tool.name.clone(),
            reason: reason.into(),
        };
        match &tool.kind {
            ToolKind::Function { strict, .. }
                if *strict && !self.tool_calling.strict_json_schema =>
            {
                Err(unsupported("provider does not support strict JSON schemas"))
            }
            ToolKind::Function { .. } => Ok(()),
            ToolKind::Freeform { .. } if !self.tool_calling.freeform_tools => {
                Err(unsupported("provider does not support freeform tools"))
            }
            ToolKind::Freeform { .. } => Ok(()),
            ToolKind::Namespace { .. } if !self.tool_calling.namespace_tools => {
                Err(unsupported("provider does not support tool namespaces"))
            }
            ToolKind::Namespace { tools } => {
                for member in tools {
                    self.ensure_tool_supported(member)?;
                }
                Ok(())
            }
            ToolKind::ToolSearch { .. } if !self.tool_calling.tool_search => {
                Err(unsupported("provider does not support tool search"))
            }
            ToolKind::ToolSearch { .. } => Ok(()),
            ToolKind::WebSearch { .. } if !self.tool_calling.web_search => {
                Err(unsupported("provider does not support web search"))
            }
            ToolKind::WebSearch { .. } => Ok(()),
        }
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
    #[serde(default)]
    pub namespace_tools: bool,
    #[serde(default)]
    pub tool_search: bool,
    #[serde(default)]
    pub web_search: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    pub encrypted_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCapabilities {
    pub prompt_cache_key: bool,
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
    /// Provider-wide operation or transport capability refusal.
    #[error("capability {capability} is unsupported: {reason}")]
    UnsupportedCapability { capability: String, reason: String },
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
    /// A configured recovery mechanism rejected the credential permanently.
    RecoveryPermanent,
    /// Recovery failed transiently. A later sampling may retry, this sampling may not.
    RecoveryTransient,
    /// This credential kind has no recovery mechanism.
    RecoveryUnavailable,
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

    /// Non-stream response derived from the canonical stream. Providers only
    /// override this when their wire exposes a genuinely different operation.
    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        let stream = self.stream(req).await?;
        futures_util::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let mut stop = StopReason::EndTurn;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
                StreamEvent::Usage { usage: value } => usage = value,
                StreamEvent::Done { stop: value } => stop = value,
                _ => {}
            }
        }
        Ok(CanonicalResponse {
            content: vec![crate::message::ContentBlock::Text { text }],
            usage,
            stop,
        })
    }

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
    fn provider_without_freeform_support_refuses_the_plan_before_any_call() {
        let mut capabilities = Capabilities {
            tools: true,
            tool_calling: ToolCallingCapabilities {
                strict_json_schema: true,
                ..ToolCallingCapabilities::default()
            },
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
            ToolSpec::freeform("exec", "run javascript", None),
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
