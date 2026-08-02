use agent_core::message::{ContentBlock, Role};
use agent_core::provider::{CanonicalRequest, ProviderError};
use aws_sdk_bedrockruntime::types::{
    InferenceConfiguration, Message, OutputConfig, SystemContentBlock, ToolConfiguration,
};

use super::request::{build_output_config, build_tool_configuration, message_to_bedrock};
use super::{AmazonBedrockProvider, DEFAULT_RETRY, invalid, invalid_request};
use agent_core::provider::Provider;

pub(super) struct BedrockRequestPlan {
    pub(super) model_id: String,
    pub(super) messages: Vec<Message>,
    pub(super) system: Vec<SystemContentBlock>,
    pub(super) inference: InferenceConfiguration,
    pub(super) tool_config: Option<ToolConfiguration>,
    pub(super) output_config: Option<OutputConfig>,
}

impl AmazonBedrockProvider {
    pub(super) fn plan(
        &self,
        request: &CanonicalRequest,
    ) -> Result<BedrockRequestPlan, ProviderError> {
        request.validate().map_err(invalid_request)?;
        let runtime = request.model_runtime.clone().map_or_else(
            || {
                self.resolve_model_runtime(
                    &request.model,
                    request.reasoning_effort.as_deref(),
                    request.max_output_tokens,
                    DEFAULT_RETRY.max_attempts.saturating_sub(1),
                    DEFAULT_RETRY.backoff_base_ms,
                )
                .map_err(invalid_request)
            },
            Ok,
        )?;
        if request.reasoning_replay {
            return Err(ProviderError::UnsupportedCapability {
                capability: "reasoning_replay".into(),
                reason: "Bedrock reasoning signatures are not canonical replay items".into(),
            });
        }
        if request.reasoning_effort.is_some() || runtime.reasoning_effort.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "reasoning".into(),
                reason: "Bedrock reasoning effort is not encoded by this adapter".into(),
            });
        }
        if request.service_tier.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "service_tier".into(),
                reason: "Bedrock service-tier selection is not configured".into(),
            });
        }
        if request.cache_key.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "prompt_cache_key".into(),
                reason: "Bedrock cache points are not canonical prompt cache keys".into(),
            });
        }
        if request.stream_options.reasoning_summary_delivery.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "reasoning_summary_delivery".into(),
                reason: "Bedrock ConverseStream has no equivalent request control".into(),
            });
        }
        self.capabilities.ensure_request_supported(request)?;
        runtime
            .ensure_tools_supported(&request.tools)
            .map_err(|error| ProviderError::UnsupportedTool {
                tool: error.tool,
                reason: error.reason,
            })?;
        let has_images = request.messages.iter().any(|message| message.has_images());
        if has_images && !runtime.accepts_images() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "vision".into(),
                reason: "the selected Bedrock account or model does not accept images".into(),
            });
        }

        let mut system = Vec::new();
        if let Some(text) = request.system.as_deref() {
            system.push(SystemContentBlock::Text(text.to_string()));
        }
        let mut messages = Vec::with_capacity(request.messages.len());
        for message in &request.messages {
            if message.role == Role::System {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } | ContentBlock::Summary { text, .. } => {
                            system.push(SystemContentBlock::Text(text.clone()));
                        }
                        _ => {
                            return Err(ProviderError::UnsupportedCapability {
                                capability: "system_content".into(),
                                reason: "Bedrock system messages accept text only".into(),
                            });
                        }
                    }
                }
                continue;
            }
            messages.push(message_to_bedrock(message)?);
        }
        if messages.is_empty() {
            return Err(invalid(
                "messages",
                "must contain at least one non-system message",
            ));
        }

        let max_tokens = i32::try_from(request.max_output_tokens)
            .map_err(|_| invalid("max_output_tokens", "exceeds the Bedrock integer range"))?;
        let inference = InferenceConfiguration::builder()
            .max_tokens(max_tokens)
            .build();
        let tool_config = (!request.tools.is_empty())
            .then(|| build_tool_configuration(&request.tools))
            .transpose()?;
        let output_config = request
            .output_schema
            .as_ref()
            .map(build_output_config)
            .transpose()?;
        Ok(BedrockRequestPlan {
            model_id: request.model.clone(),
            messages,
            system,
            inference,
            tool_config,
            output_config,
        })
    }
}
