use std::time::Duration;

use agent_core::provider::{ProviderError, ResponseMetadata, StreamEvent};
use aws_types::request_id::RequestId;
use futures_util::stream::BoxStream;

use super::events::{BedrockEventMapper, map_service_error, map_stream_error};
use super::plan::BedrockRequestPlan;

pub(super) async fn execute(
    client: &aws_sdk_bedrockruntime::Client,
    idle: Duration,
    plan: BedrockRequestPlan,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let call = client
        .converse_stream()
        .model_id(plan.model_id)
        .set_messages(Some(plan.messages))
        .set_system((!plan.system.is_empty()).then_some(plan.system))
        .inference_config(plan.inference)
        .set_tool_config(plan.tool_config)
        .set_output_config(plan.output_config);
    let output = call.send().await.map_err(|error| {
        if let Some(service) = error.as_service_error() {
            map_service_error(service)
        } else {
            ProviderError::Transport("Bedrock ConverseStream request failed".into())
        }
    })?;
    let request_id = output.request_id().map(str::to_string);
    let mut receiver = output.stream;
    let stream = async_stream::stream! {
        if request_id.is_some() {
            yield Ok(StreamEvent::ResponseMetadata {
                metadata: Box::new(ResponseMetadata {
                    request_id,
                    ..ResponseMetadata::default()
                }),
            });
        }
        let mut mapper = BedrockEventMapper::default();
        loop {
            match tokio::time::timeout(idle, receiver.recv()).await {
                Err(_) => {
                    yield Err(ProviderError::Stream("Bedrock stream idle timeout".into()));
                    return;
                }
                Ok(Ok(Some(event))) => {
                    for event in mapper.ingest(event) {
                        let terminal = matches!(event, Ok(StreamEvent::Done { .. }));
                        yield event;
                        if terminal {
                            return;
                        }
                    }
                }
                Ok(Ok(None)) => {
                    if let Some(done) = mapper.finish() {
                        yield done;
                    } else {
                        yield Err(ProviderError::Stream("Bedrock stream ended without a stop reason".into()));
                    }
                    return;
                }
                Ok(Err(error)) => {
                    let mapped = error
                        .as_service_error()
                        .map(map_stream_error)
                        .unwrap_or_else(|| {
                            ProviderError::Stream("Bedrock event stream transport failed".into())
                        });
                    yield Err(mapped);
                    return;
                }
            }
        }
    };
    Ok(Box::pin(stream))
}
