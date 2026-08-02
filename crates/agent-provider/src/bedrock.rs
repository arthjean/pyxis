//! Amazon Bedrock Runtime adapter using the official `ConverseStream` API.

use std::sync::RwLock;

use agent_auth::provider::ProviderCredential;
use agent_core::model::{ModelRetryPolicy, ModelRuntimeError, ResolvedModelRuntime};
use agent_core::provider::{
    AuthError, CanonicalRequest, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderErrorCategory, ProviderKind, StreamEvent,
};
use async_trait::async_trait;
use futures_util::stream::BoxStream;

use crate::models::{CatalogModel, ModelCatalog};

mod auth;
mod config;
mod events;
mod plan;
mod request;
mod stream;

pub use auth::{BedrockAccountState, BedrockCredentialSource};
use auth::{api_key_client, sdk_chain_client};
pub use config::AmazonBedrockConfig;

const DEFAULT_RETRY: ModelRetryPolicy = ModelRetryPolicy {
    max_attempts: 3,
    backoff_base_ms: 100,
};

pub struct AmazonBedrockProvider {
    client: aws_sdk_bedrockruntime::Client,
    config: AmazonBedrockConfig,
    capabilities: Capabilities,
    catalog: RwLock<ModelCatalog>,
    account: BedrockAccountState,
}

impl AmazonBedrockProvider {
    /// Resolves the AWS SDK credential chain before constructing the provider.
    /// No OpenAI header or bearer credential enters this adapter.
    pub async fn new(config: AmazonBedrockConfig) -> Result<Self, ProviderError> {
        config.validate()?;
        let client = sdk_chain_client(&config).await?;
        Self::from_client_with_source(config, client, BedrockCredentialSource::AwsSdkChain, None)
    }

    /// Constructs a direct ConverseStream client using an Amazon Bedrock API
    /// key. The official SDK owns bearer authentication and request framing.
    pub async fn new_with_api_key(
        config: AmazonBedrockConfig,
        credential: ProviderCredential,
    ) -> Result<Self, ProviderError> {
        config.validate()?;
        let (client, identity_fingerprint) = api_key_client(&config, &credential).await?;
        Self::from_client_with_source(
            config,
            client,
            BedrockCredentialSource::BedrockApiKey,
            Some(identity_fingerprint),
        )
    }

    /// Constructs the adapter around an already configured official SDK client.
    /// Useful for custom AWS credential providers and deterministic tests.
    pub fn from_client(
        config: AmazonBedrockConfig,
        client: aws_sdk_bedrockruntime::Client,
    ) -> Result<Self, ProviderError> {
        Self::from_client_with_source(
            config,
            client,
            BedrockCredentialSource::InjectedSdkClient,
            None,
        )
    }

    fn from_client_with_source(
        config: AmazonBedrockConfig,
        client: aws_sdk_bedrockruntime::Client,
        credential_source: BedrockCredentialSource,
        identity_fingerprint: Option<String>,
    ) -> Result<Self, ProviderError> {
        config.validate()?;
        let catalog = ModelCatalog::from_static(config.models.clone())
            .map_err(|error| invalid("models", error.to_string()))?;
        let account = BedrockAccountState {
            region: config.region.clone(),
            profile: config.profile.clone(),
            credential_source,
            identity_fingerprint,
            preferred_models: config.preferred_models.clone(),
        };
        Ok(Self {
            client,
            capabilities: config.capabilities.clone(),
            config,
            catalog: RwLock::new(catalog),
            account,
        })
    }

    pub fn account_state(&self) -> &BedrockAccountState {
        &self.account
    }

    pub fn list_models(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        self.catalog
            .read()
            .map(|catalog| catalog.models())
            .map_err(|_| ProviderError::Decode("bedrock catalog lock poisoned".into()))
    }

    pub async fn preconnect_websocket(&self) -> Result<(), ProviderError> {
        Err(ProviderError::UnsupportedCapability {
            capability: "websocket".into(),
            reason: "Amazon Bedrock ConverseStream is an HTTP event stream".into(),
        })
    }

    fn catalog_window(&self, model: &str) -> Option<u32> {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.context_window(model.trim()))
    }
}

#[async_trait]
impl Provider for AmazonBedrockProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AmazonBedrock
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn max_context_for_model(&self, model: &str) -> u32 {
        self.catalog_window(model)
            .unwrap_or(self.capabilities.max_context)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.catalog_window(model)
    }

    fn resolve_model_runtime(
        &self,
        model: &str,
        reasoning_effort: Option<&str>,
        max_output_tokens: u32,
        max_retries: u32,
        backoff_base_ms: u64,
    ) -> Result<ResolvedModelRuntime, ModelRuntimeError> {
        self.catalog
            .read()
            .map_err(|_| ModelRuntimeError::InvalidField {
                field: "catalog",
                detail: "lock poisoned".into(),
            })?
            .resolve(
                model,
                reasoning_effort,
                max_output_tokens,
                ModelRetryPolicy {
                    max_attempts: max_retries.saturating_add(1),
                    backoff_base_ms,
                },
            )
    }

    async fn stream(
        &self,
        request: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let plan = self.plan(&request)?;
        stream::execute(&self.client, self.config.idle_timeout, plan).await
    }

    fn classify_error(&self, error: &ProviderError) -> ErrorClass {
        match error {
            ProviderError::Credential(error) => ErrorClass::Auth(*error),
            ProviderError::Api {
                category, status, ..
            } => match category {
                ProviderErrorCategory::Authentication | ProviderErrorCategory::PermissionDenied => {
                    ErrorClass::Auth(AuthError::Invalid)
                }
                ProviderErrorCategory::RateLimited => ErrorClass::RateLimited,
                ProviderErrorCategory::Overloaded => ErrorClass::Overloaded(status.unwrap_or(503)),
                ProviderErrorCategory::Failed | ProviderErrorCategory::Incomplete => {
                    ErrorClass::Retryable
                }
                _ => ErrorClass::InvalidRequest,
            },
            ProviderError::Http { status: 429, .. } => ErrorClass::RateLimited,
            ProviderError::Http { status, .. } if *status >= 500 => ErrorClass::Retryable,
            ProviderError::Transport(_) | ProviderError::Stream(_) | ProviderError::Decode(_) => {
                ErrorClass::Retryable
            }
            ProviderError::Http { .. }
            | ProviderError::UnsupportedTool { .. }
            | ProviderError::UnsupportedCapability { .. }
            | ProviderError::ContextLengthExceeded => ErrorClass::InvalidRequest,
        }
    }

    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        Err(ProviderError::Credential(AuthError::RecoveryUnavailable))
    }
}

fn valid_aws_region(region: &str) -> bool {
    // Region identifiers become a DNS label in the SDK endpoint. Bound both
    // the complete label and every component before walking attacker input.
    if !(3..=63).contains(&region.len()) {
        return false;
    }
    let mut segments = region.split('-');
    let Some(partition) = segments.next() else {
        return false;
    };
    if !(2..=4).contains(&partition.len())
        || !partition.bytes().all(|byte| byte.is_ascii_lowercase())
    {
        return false;
    }
    let remaining = segments.collect::<Vec<_>>();
    let Some((number, names)) = remaining.split_last() else {
        return false;
    };
    !names.is_empty()
        && names.iter().all(|segment| {
            (1..=16).contains(&segment.len())
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
        && (1..=3).contains(&number.len())
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number != &"0"
}

fn invalid(field: &'static str, detail: impl Into<String>) -> ProviderError {
    invalid_request(format!(
        "invalid Bedrock field `{field}`: {}",
        detail.into()
    ))
}

fn invalid_request(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::Api {
        category: ProviderErrorCategory::InvalidRequest,
        status: None,
        message: agent_core::redaction::redact_text(&error.to_string()),
        retry_after_ms: None,
        request_id: None,
        auth_request_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::events::{BedrockEventMapper, map_stream_error};
    use super::*;
    use agent_core::message::Message as CanonicalMessage;
    use agent_core::model::{
        InputModality, ModelDescriptor, ModelToolMode, MultiAgentVersion, ReasoningReplaySupport,
        ResponsesDialect, TruncationMode, TruncationPolicy,
    };
    use agent_core::provider::{
        CapabilityLimits, OutputSchema, StopReason, ToolCallingCapabilities, ToolSpec,
    };
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent,
        ContentBlockStopEvent, ConverseStreamMetadataEvent, ConverseStreamOutput, MessageStopEvent,
        StopReason as AwsStopReason, TokenUsage as AwsTokenUsage, ToolUseBlockDelta,
        ToolUseBlockStart,
    };
    use aws_types::region::Region;

    fn descriptor() -> ModelDescriptor {
        ModelDescriptor {
            slug: "anthropic.claude-test-v1:0".into(),
            display_name: "Claude test".into(),
            instructions: "Be useful.".into(),
            context_window: 8_192,
            auto_compact_token_limit: 7_000,
            input_modalities: vec![InputModality::Text],
            supports_reasoning: false,
            default_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
            supports_verbosity: false,
            default_verbosity: None,
            supports_parallel_tool_calls: true,
            tool_capabilities: Default::default(),
            service_tiers: Vec::new(),
            reasoning_replay: ReasoningReplaySupport::Disabled,
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
            multi_agent_version: MultiAgentVersion::Disabled,
            truncation: TruncationPolicy {
                mode: TruncationMode::Tokens,
                limit: 1_000,
            },
            comp_hash: None,
        }
    }

    fn capable_config() -> AmazonBedrockConfig {
        let capabilities = Capabilities {
            tools: true,
            structured_output: true,
            max_context: 8_192,
            limits: CapabilityLimits {
                max_tool_schema_bytes: Some(64 * 1024),
                ..CapabilityLimits::default()
            },
            tool_calling: ToolCallingCapabilities {
                strict_json_schema: true,
                parallel_tool_calls: true,
                ..ToolCallingCapabilities::default()
            },
            ..Capabilities::default()
        };
        AmazonBedrockConfig::new("eu-west-3", vec![descriptor()])
            .unwrap()
            .with_preferred_models(vec!["anthropic.claude-test-v1:0".into()])
            .unwrap()
            .with_capabilities(capabilities)
            .unwrap()
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            model: "anthropic.claude-test-v1:0".into(),
            messages: vec![CanonicalMessage::user("hello")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        }
    }

    #[test]
    fn text_tool_usage_and_stop_map_to_canonical_events() {
        let mut mapper = BedrockEventMapper::default();
        let text = mapper.ingest(ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::Text("hello".into()))
                .build()
                .unwrap(),
        ));
        assert!(matches!(
            &text[0],
            Ok(StreamEvent::TextDelta { text }) if text == "hello"
        ));
        let start = ToolUseBlockStart::builder()
            .tool_use_id("call-1")
            .name("read")
            .build()
            .unwrap();
        assert!(matches!(
            mapper.ingest(ConverseStreamOutput::ContentBlockStart(
                ContentBlockStartEvent::builder()
                    .content_block_index(1)
                    .start(ContentBlockStart::ToolUse(start))
                    .build()
                    .unwrap()
            ))[0],
            Ok(StreamEvent::ToolCallStart { .. })
        ));
        let delta = ToolUseBlockDelta::builder()
            .input("{\"path\":")
            .build()
            .unwrap();
        assert!(matches!(
            mapper.ingest(ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(1)
                    .delta(ContentBlockDelta::ToolUse(delta))
                    .build()
                    .unwrap()
            ))[0],
            Ok(StreamEvent::ToolCallDelta { .. })
        ));
        assert!(matches!(
            mapper.ingest(ConverseStreamOutput::ContentBlockStop(
                ContentBlockStopEvent::builder()
                    .content_block_index(1)
                    .build()
                    .unwrap()
            ))[0],
            Ok(StreamEvent::ToolCallEnd { .. })
        ));
        mapper.ingest(ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(AwsStopReason::ToolUse)
                .build()
                .unwrap(),
        ));
        let usage = AwsTokenUsage::builder()
            .input_tokens(10)
            .output_tokens(4)
            .total_tokens(14)
            .build()
            .unwrap();
        let events = mapper.ingest(ConverseStreamOutput::Metadata(
            ConverseStreamMetadataEvent::builder().usage(usage).build(),
        ));
        assert!(matches!(events[0], Ok(StreamEvent::Usage { .. })));
        assert!(matches!(
            events[1],
            Ok(StreamEvent::Done {
                stop: StopReason::ToolUse
            })
        ));
    }

    #[tokio::test]
    async fn request_features_are_capability_gated_and_built() {
        let shared = aws_sdk_bedrockruntime::Config::builder()
            .region(Region::new("eu-west-3"))
            .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
            .build();
        let provider = AmazonBedrockProvider::from_client(
            capable_config(),
            aws_sdk_bedrockruntime::Client::from_conf(shared),
        )
        .unwrap();
        let mut request = request();
        request.tools = vec![ToolSpec::function(
            "read",
            "read a file",
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        )];
        request.output_schema = Some(OutputSchema {
            name: "answer".into(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });
        let plan = provider.plan(&request).unwrap();
        assert!(plan.tool_config.is_some());
        assert!(plan.output_config.is_some());

        let disabled = AmazonBedrockConfig::new("eu-west-3", vec![descriptor()]).unwrap();
        let provider =
            AmazonBedrockProvider::from_client(disabled, provider.client.clone()).unwrap();
        assert!(matches!(
            provider.plan(&request),
            Err(ProviderError::UnsupportedCapability { .. })
        ));
        assert!(matches!(
            provider.preconnect_websocket().await,
            Err(ProviderError::UnsupportedCapability { .. })
        ));
    }

    #[test]
    fn validation_stream_exception_is_typed_and_nonretryable() {
        let error = aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("ValidationException")
                .message("do not echo this backend body")
                .build(),
        );
        let mapped = map_stream_error(&error);
        assert!(matches!(
            mapped,
            ProviderError::Api {
                category: ProviderErrorCategory::InvalidRequest,
                status: Some(400),
                ..
            }
        ));
        assert!(!mapped.to_string().contains("do not echo"));
    }

    #[test]
    fn invalid_account_and_websocket_are_explicit() {
        let oversized = format!("eu-{}-1", "a".repeat(64));
        for region in ["not a region", "💣", "eu-west-x", "a:b", &oversized] {
            assert!(
                AmazonBedrockConfig::new(region, vec![descriptor()]).is_err(),
                "accepted invalid region {region}"
            );
        }
        assert!(
            capable_config()
                .with_preferred_models(vec!["missing".into()])
                .is_err()
        );

        let unsupported = Capabilities {
            reasoning: true,
            max_context: 8_192,
            ..Capabilities::default()
        };
        assert!(
            AmazonBedrockConfig::new("eu-west-3", vec![descriptor()])
                .unwrap()
                .with_capabilities(unsupported)
                .is_err()
        );
    }
}
