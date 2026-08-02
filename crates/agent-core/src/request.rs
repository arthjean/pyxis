//! Provider-neutral model request and its local validation boundary.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::model::ResolvedModelRuntime;
use crate::provider::{ToolSpec, ToolSpecValidationError};
use crate::redaction::{is_sensitive_key, looks_like_signed_url};

/// Canonical correlation key used by adapters whose transport state must not
/// cross a runtime turn boundary.
pub const TURN_ID_METADATA_KEY: &str = "turn_id";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSchema {
    pub name: String,
    pub schema: serde_json::Value,
    pub strict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryDelivery {
    SequentialCutoff,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequestStreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary_delivery: Option<ReasoningSummaryDelivery>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_usage: bool,
}

impl RequestStreamOptions {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Canonical request produced at the prompt boundary.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_runtime: Option<ResolvedModelRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning_replay: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<OutputSchema>,
    #[serde(default, skip_serializing_if = "RequestStreamOptions::is_empty")]
    pub stream_options: RequestStreamOptions,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub client_metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
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
        if self
            .service_tier
            .as_ref()
            .is_some_and(|tier| tier.trim().is_empty() || tier.len() > 64)
        {
            return Err(CanonicalRequestValidationError::InvalidServiceTier);
        }
        if let Some(output) = &self.output_schema {
            if output.name.trim().is_empty() || output.name.len() > 64 {
                return Err(CanonicalRequestValidationError::InvalidOutputSchema {
                    detail: "name must contain 1..=64 bytes".to_string(),
                });
            }
            if !output.schema.is_object() {
                return Err(CanonicalRequestValidationError::InvalidOutputSchema {
                    detail: "schema must be a JSON object".to_string(),
                });
            }
        }
        validate_client_metadata(&self.client_metadata)?;
        if let Some(runtime) = &self.model_runtime {
            runtime.validate().map_err(|error| {
                CanonicalRequestValidationError::InvalidModelRuntime {
                    detail: error.to_string(),
                }
            })?;
            if runtime.slug != self.model {
                return Err(CanonicalRequestValidationError::InvalidModelRuntime {
                    detail: "runtime slug does not match request model".into(),
                });
            }
            if runtime.max_output_tokens != self.max_output_tokens {
                return Err(CanonicalRequestValidationError::InvalidModelRuntime {
                    detail: "runtime output limit does not match request".into(),
                });
            }
            if runtime.reasoning_effort != self.reasoning_effort {
                return Err(CanonicalRequestValidationError::InvalidModelRuntime {
                    detail: "runtime reasoning effort does not match request".into(),
                });
            }
            if let Some(tier) = self.service_tier.as_deref()
                && tier != "default"
                && !runtime
                    .service_tiers
                    .iter()
                    .any(|supported| supported.eq_ignore_ascii_case(tier))
            {
                return Err(CanonicalRequestValidationError::UnsupportedServiceTier {
                    model: runtime.slug.clone(),
                    tier: tier.to_string(),
                });
            }
            if self.stream_options.reasoning_summary_delivery.is_some()
                && runtime.reasoning_effort.is_none()
            {
                return Err(CanonicalRequestValidationError::IncompatibleControl {
                    model: runtime.slug.clone(),
                    control: "reasoning_summary_delivery".to_string(),
                });
            }
            if self.reasoning_replay
                && runtime.reasoning_replay != crate::model::ReasoningReplaySupport::Enabled
            {
                return Err(
                    CanonicalRequestValidationError::UnsupportedReasoningReplay {
                        model: runtime.slug.clone(),
                    },
                );
            }
            if !runtime.accepts_images()
                && self.messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|block| matches!(block, crate::message::ContentBlock::Image { .. }))
                })
            {
                return Err(CanonicalRequestValidationError::UnsupportedImageModality {
                    model: runtime.slug.clone(),
                });
            }
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
        if self.tools.iter().any(ToolSpec::is_deferred)
            && !self
                .tools
                .iter()
                .any(|tool| matches!(tool.kind, crate::provider::ToolKind::ToolSearch { .. }))
        {
            return Err(CanonicalRequestValidationError::MissingDeferredToolSearch);
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
    #[error("service tier must contain 1..=64 bytes")]
    InvalidServiceTier,
    #[error("invalid output schema: {detail}")]
    InvalidOutputSchema { detail: String },
    #[error("invalid client metadata: {detail}")]
    InvalidClientMetadata { detail: String },
    #[error("invalid model runtime: {detail}")]
    InvalidModelRuntime { detail: String },
    #[error("model {model} does not accept image input")]
    UnsupportedImageModality { model: String },
    #[error("model {model} does not support encrypted stateless reasoning replay")]
    UnsupportedReasoningReplay { model: String },
    #[error("model {model} does not support service tier {tier}")]
    UnsupportedServiceTier { model: String, tier: String },
    #[error("model {model} does not support request control {control}")]
    IncompatibleControl { model: String, control: String },
    #[error("message {index} is invalid: {detail}")]
    InvalidMessage { index: usize, detail: String },
    #[error("tool spec is invalid: {0}")]
    InvalidTool(#[from] ToolSpecValidationError),
    #[error("duplicate tool name: {tool}")]
    DuplicateToolName { tool: String },
    #[error("deferred tools require a tool_search reference in the same catalog")]
    MissingDeferredToolSearch,
}

fn validate_client_metadata(
    metadata: &BTreeMap<String, String>,
) -> Result<(), CanonicalRequestValidationError> {
    if metadata.len() > 64 {
        return Err(CanonicalRequestValidationError::InvalidClientMetadata {
            detail: "at most 64 entries are allowed".to_string(),
        });
    }
    for (key, value) in metadata {
        if key.is_empty() || key.len() > 128 || value.len() > 1024 {
            return Err(CanonicalRequestValidationError::InvalidClientMetadata {
                detail: format!("field `{key}` exceeds its key/value bound"),
            });
        }
        if is_sensitive_key(key) {
            return Err(CanonicalRequestValidationError::InvalidClientMetadata {
                detail: format!("sensitive field `{key}` is not allowed"),
            });
        }
        if looks_like_signed_url(value) {
            return Err(CanonicalRequestValidationError::InvalidClientMetadata {
                detail: format!("signed URL in field `{key}` is not allowed"),
            });
        }
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::model::{
        InputModality, ModelRetryPolicy, ModelRuntimeSource, ModelToolMode, ResponsesDialect,
        TruncationMode, TruncationPolicy,
    };

    fn text_only_runtime() -> ResolvedModelRuntime {
        ResolvedModelRuntime {
            slug: "text-model".into(),
            source: ModelRuntimeSource::Embedded {
                version: "test".into(),
            },
            instructions: "test".into(),
            fingerprint: "a".repeat(64),
            context_window: 10_000,
            auto_compact_token_limit: 8_000,
            input_modalities: vec![InputModality::Text],
            reasoning_effort: None,
            supports_verbosity: false,
            verbosity: None,
            supports_parallel_tool_calls: false,
            tool_capabilities: crate::model::ModelToolCapabilities::default(),
            service_tiers: Vec::new(),
            reasoning_replay: crate::model::ReasoningReplaySupport::Disabled,
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
            multi_agent_version: crate::model::MultiAgentVersion::Disabled,
            truncation: TruncationPolicy {
                mode: TruncationMode::Tokens,
                limit: 1_000,
            },
            retry: ModelRetryPolicy {
                max_attempts: 2,
                backoff_base_ms: 50,
            },
            max_output_tokens: 100,
            comp_hash: None,
        }
    }

    #[test]
    fn validation_rejects_invalid_messages_tools_and_modalities() {
        let invalid_message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "out".into(),
                status: None,
                structured_content: None,
                untrusted: true,
                is_error: false,
                error_kind: None,
                duration_ms: None,
                truncation: None,
                execution: None,
                images: Vec::new(),
            }],
        };
        let request = CanonicalRequest {
            model: "gpt".into(),
            messages: vec![invalid_message],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::InvalidMessage { .. })
        ));

        let request = CanonicalRequest {
            model: "gpt".into(),
            messages: vec![Message::user("ok")],
            tools: vec![ToolSpec::function(
                "bad",
                String::new(),
                serde_json::json!({
                    "type": "string",
                    "additionalProperties": false,
                    "required": []
                }),
            )],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::InvalidTool(
                ToolSpecValidationError::SchemaMustBeObject { .. }
            ))
        ));

        let request = CanonicalRequest {
            model: "text-model".into(),
            model_runtime: Some(text_only_runtime()),
            system: Some("test".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                }],
            }],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::UnsupportedImageModality { .. })
        ));
    }

    #[test]
    fn validation_rejects_non_strict_tools_and_duplicate_names() {
        let request = CanonicalRequest {
            model: "gpt".into(),
            messages: vec![Message::user("ok")],
            tools: vec![ToolSpec::function(
                "read",
                "lit",
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            )],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::InvalidTool(
                ToolSpecValidationError::SchemaMustDenyAdditionalProperties { .. }
            ))
        ));

        let strict_tool = ToolSpec::function(
            "read",
            "lit",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        );
        let request = CanonicalRequest {
            model: "gpt".into(),
            messages: vec![Message::user("ok")],
            tools: vec![strict_tool.clone(), strict_tool],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::DuplicateToolName { tool }) if tool == "read"
        ));
    }

    #[test]
    fn deferred_tools_require_a_search_reference() {
        let deferred = ToolSpec::function_with_options(
            "deferred",
            "loaded by search",
            serde_json::json!({"type": "object", "properties": {}}),
            false,
            true,
            None,
        );
        let request = CanonicalRequest {
            model: "gpt".into(),
            messages: vec![Message::user("ok")],
            tools: vec![deferred.clone()],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            request.validate(),
            Err(CanonicalRequestValidationError::MissingDeferredToolSearch)
        ));

        let valid = CanonicalRequest {
            tools: vec![
                deferred,
                ToolSpec::tool_search(
                    "find tools",
                    "client",
                    serde_json::json!({"type": "object", "properties": {}}),
                ),
            ],
            ..request
        };
        valid
            .validate()
            .expect("search resolves deferred references");
    }

    #[test]
    fn incompatible_enriched_controls_fail_at_the_canonical_boundary() {
        let mut runtime = text_only_runtime();
        runtime.service_tiers = vec!["priority".into()];
        let base = CanonicalRequest {
            model: runtime.slug.clone(),
            model_runtime: Some(runtime),
            messages: vec![Message::user("ok")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };

        let unsupported_tier = CanonicalRequest {
            service_tier: Some("flex".into()),
            ..base.clone()
        };
        assert!(matches!(
            unsupported_tier.validate(),
            Err(CanonicalRequestValidationError::UnsupportedServiceTier { tier, .. })
                if tier == "flex"
        ));

        let unsupported_summary = CanonicalRequest {
            stream_options: RequestStreamOptions {
                reasoning_summary_delivery: Some(ReasoningSummaryDelivery::SequentialCutoff),
                include_usage: false,
            },
            ..base.clone()
        };
        assert!(matches!(
            unsupported_summary.validate(),
            Err(CanonicalRequestValidationError::IncompatibleControl { control, .. })
                if control == "reasoning_summary_delivery"
        ));

        let invalid_schema = CanonicalRequest {
            output_schema: Some(OutputSchema {
                name: "result".into(),
                schema: serde_json::json!(["not", "an", "object"]),
                strict: true,
            }),
            ..base
        };
        assert!(matches!(
            invalid_schema.validate(),
            Err(CanonicalRequestValidationError::InvalidOutputSchema { .. })
        ));
    }

    #[test]
    fn enriched_controls_round_trip_without_collapsing_fields() {
        let request = CanonicalRequest {
            model: "gpt".into(),
            system: Some(String::new()),
            service_tier: Some("priority".into()),
            output_schema: Some(OutputSchema {
                name: "answer".into(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false
                }),
                strict: true,
            }),
            stream_options: RequestStreamOptions {
                reasoning_summary_delivery: Some(ReasoningSummaryDelivery::SequentialCutoff),
                include_usage: true,
            },
            client_metadata: BTreeMap::from([
                ("window_id".into(), "window-1".into()),
                ("turn_kind".into(), "user".into()),
            ]),
            cache_key: Some("thread-1".into()),
            messages: vec![Message::user("hello")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        request.validate().unwrap();

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["system"], "");
        assert_eq!(value["service_tier"], "priority");
        assert_eq!(value["output_schema"]["strict"], true);
        assert_eq!(
            value["stream_options"]["reasoning_summary_delivery"],
            "sequential_cutoff"
        );
        assert_eq!(value["client_metadata"]["window_id"], "window-1");
        assert_eq!(value["cache_key"], "thread-1");
        assert_eq!(
            serde_json::from_value::<CanonicalRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn old_requests_default_additive_controls_without_rewriting_them() {
        let request: CanonicalRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt",
            "reasoning_replay": false,
            "messages": [],
            "tools": [],
            "max_output_tokens": 100
        }))
        .unwrap();
        assert_eq!(request.service_tier, None);
        assert_eq!(request.output_schema, None);
        assert_eq!(request.system, None);
        assert_eq!(request.stream_options, RequestStreamOptions::default());
        assert!(request.client_metadata.is_empty());
        assert_eq!(request.cache_key, None);
        let current = serde_json::to_value(request).unwrap();
        for field in [
            "service_tier",
            "output_schema",
            "stream_options",
            "client_metadata",
            "cache_key",
        ] {
            assert!(current.get(field).is_none(), "default {field} stays absent");
        }
    }

    #[test]
    fn client_metadata_refuses_sensitive_fields_before_provider_access() {
        for key in ["access_token", "session_token", "password", "apikey"] {
            let request = CanonicalRequest {
                model: "gpt".into(),
                client_metadata: BTreeMap::from([(key.into(), "secret".into())]),
                messages: vec![Message::user("hello")],
                max_output_tokens: 100,
                ..CanonicalRequest::default()
            };
            assert!(matches!(
                request.validate(),
                Err(CanonicalRequestValidationError::InvalidClientMetadata { detail })
                    if detail.contains(key) && !detail.contains("secret")
            ));
        }

        let request = CanonicalRequest {
            model: "gpt".into(),
            client_metadata: BTreeMap::from([(
                "asset".into(),
                "https://uploads.invalid/a?X-Amz-Signature=secret".into(),
            )]),
            messages: vec![Message::user("hello")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        let error = request.validate().unwrap_err().to_string();
        assert!(error.contains("signed URL"));
        assert!(!error.contains("secret"));
    }
}
