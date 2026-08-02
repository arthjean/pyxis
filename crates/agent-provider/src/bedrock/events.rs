//! Bedrock event and service-error mapping into canonical provider events.

use std::collections::HashMap;

use agent_core::provider::{
    ProviderError, ProviderErrorCategory, StopReason, StreamEvent, TokenUsage,
};
use aws_sdk_bedrockruntime::types::{ContentBlockDelta, ContentBlockStart, ConverseStreamOutput};
use aws_types::request_id::RequestId;

#[derive(Default)]
pub(super) struct BedrockEventMapper {
    tool_calls: HashMap<i32, String>,
    pending_stop: Option<StopReason>,
}

impl BedrockEventMapper {
    pub(super) fn ingest(
        &mut self,
        event: ConverseStreamOutput,
    ) -> Vec<Result<StreamEvent, ProviderError>> {
        match event {
            ConverseStreamOutput::ContentBlockStart(event) => match event.start() {
                Some(ContentBlockStart::ToolUse(tool)) => {
                    let id = tool.tool_use_id().to_string();
                    self.tool_calls
                        .insert(event.content_block_index(), id.clone());
                    vec![Ok(StreamEvent::tool_call_start(id, tool.name()))]
                }
                Some(_) => vec![Ok(unmapped("bedrock.content_block_start"))],
                None => Vec::new(),
            },
            ConverseStreamOutput::ContentBlockDelta(event) => match event.delta() {
                Some(ContentBlockDelta::Text(text)) => {
                    vec![Ok(StreamEvent::TextDelta { text: text.clone() })]
                }
                Some(ContentBlockDelta::ToolUse(delta)) => self
                    .tool_calls
                    .get(&event.content_block_index())
                    .cloned()
                    .map(|id| {
                        vec![Ok(StreamEvent::ToolCallDelta {
                            id,
                            input_delta: delta.input().to_string(),
                        })]
                    })
                    .unwrap_or_else(|| {
                        vec![Err(ProviderError::Decode(
                            "Bedrock tool delta preceded its start".into(),
                        ))]
                    }),
                Some(ContentBlockDelta::ReasoningContent(reasoning)) => reasoning
                    .as_text()
                    .ok()
                    .map(|text| vec![Ok(StreamEvent::ReasoningDelta { text: text.clone() })])
                    .unwrap_or_else(|| vec![Ok(unmapped("bedrock.reasoning_delta"))]),
                Some(_) => vec![Ok(unmapped("bedrock.content_block_delta"))],
                None => Vec::new(),
            },
            ConverseStreamOutput::ContentBlockStop(event) => self
                .tool_calls
                .remove(&event.content_block_index())
                .map(|id| vec![Ok(StreamEvent::ToolCallEnd { id })])
                .unwrap_or_default(),
            ConverseStreamOutput::MessageStop(event) => {
                match bedrock_stop_reason(event.stop_reason().as_str()) {
                    Ok(stop) => {
                        self.pending_stop = Some(stop);
                        Vec::new()
                    }
                    Err(error) => vec![Err(error)],
                }
            }
            ConverseStreamOutput::Metadata(event) => {
                let mut events = Vec::new();
                if let Some(usage) = event.usage() {
                    events.push(Ok(StreamEvent::Usage {
                        usage: TokenUsage {
                            input: nonnegative(usage.input_tokens()),
                            cached_input: nonnegative(
                                usage.cache_read_input_tokens().unwrap_or_default(),
                            ),
                            cache_write_input: nonnegative(
                                usage.cache_write_input_tokens().unwrap_or_default(),
                            ),
                            output: nonnegative(usage.output_tokens()),
                            reasoning_output: 0,
                            total: nonnegative(usage.total_tokens()),
                        },
                    }));
                }
                if let Some(stop) = self.pending_stop.take() {
                    events.push(Ok(StreamEvent::Done { stop }));
                }
                events
            }
            ConverseStreamOutput::MessageStart(_) => Vec::new(),
            _ => vec![Ok(unmapped("bedrock.stream_event"))],
        }
    }

    pub(super) fn finish(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        self.pending_stop
            .take()
            .map(|stop| Ok(StreamEvent::Done { stop }))
    }
}

fn bedrock_stop_reason(reason: &str) -> Result<StopReason, ProviderError> {
    match reason {
        "end_turn" => Ok(StopReason::EndTurn),
        "tool_use" => Ok(StopReason::ToolUse),
        "max_tokens" | "model_context_window_exceeded" => Ok(StopReason::MaxTokens),
        "content_filtered" | "guardrail_intervened" => Ok(StopReason::ContentFilter),
        "stop_sequence" => Ok(StopReason::StopSequence),
        "malformed_model_output" | "malformed_tool_use" => Err(api_error(
            ProviderErrorCategory::InvalidRequest,
            Some(400),
            "Bedrock model returned malformed structured output",
            None,
        )),
        _ => Ok(StopReason::IncompleteUnknown),
    }
}

pub(super) fn map_stream_error(
    error: &aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
) -> ProviderError {
    use aws_smithy_types::error::metadata::ProvideErrorMetadata;
    let code = error.code().unwrap_or_default();
    let request_id = error.request_id().map(str::to_string);
    if error.is_validation_exception() || code == "ValidationException" {
        api_error(
            ProviderErrorCategory::InvalidRequest,
            Some(400),
            "Bedrock validation rejected the request",
            request_id,
        )
    } else if error.is_throttling_exception() || code == "ThrottlingException" {
        api_error(
            ProviderErrorCategory::RateLimited,
            Some(429),
            "Bedrock throttled the stream",
            request_id,
        )
    } else if error.is_service_unavailable_exception() || code == "ServiceUnavailableException" {
        api_error(
            ProviderErrorCategory::Overloaded,
            Some(503),
            "Bedrock stream service unavailable",
            request_id,
        )
    } else {
        api_error(
            ProviderErrorCategory::Failed,
            Some(500),
            "Bedrock stream failed",
            request_id,
        )
    }
}

pub(super) fn map_service_error(
    error: &aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError,
) -> ProviderError {
    let request_id = error.request_id().map(str::to_string);
    if error.is_access_denied_exception() {
        api_error(
            ProviderErrorCategory::PermissionDenied,
            Some(403),
            "Bedrock access denied",
            request_id,
        )
    } else if error.is_validation_exception() || error.is_resource_not_found_exception() {
        api_error(
            ProviderErrorCategory::InvalidRequest,
            Some(400),
            "Bedrock rejected the request",
            request_id,
        )
    } else if error.is_throttling_exception() {
        api_error(
            ProviderErrorCategory::RateLimited,
            Some(429),
            "Bedrock throttled the request",
            request_id,
        )
    } else if error.is_service_unavailable_exception() || error.is_model_not_ready_exception() {
        api_error(
            ProviderErrorCategory::Overloaded,
            Some(503),
            "Bedrock model unavailable",
            request_id,
        )
    } else {
        api_error(
            ProviderErrorCategory::Failed,
            Some(500),
            "Bedrock request failed",
            request_id,
        )
    }
}

fn api_error(
    category: ProviderErrorCategory,
    status: Option<u16>,
    message: &str,
    request_id: Option<String>,
) -> ProviderError {
    ProviderError::Api {
        category,
        status,
        message: message.into(),
        retry_after_ms: None,
        request_id,
        auth_request_id: None,
    }
}

fn unmapped(item_type: &str) -> StreamEvent {
    StreamEvent::UnmappedItem {
        item_type: item_type.into(),
        extension: None,
    }
}

fn nonnegative(value: i32) -> u64 {
    u64::try_from(value).unwrap_or_default()
}
