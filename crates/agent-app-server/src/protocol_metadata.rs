//! External projection of provider-neutral response metadata.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Provider-neutral API error category exposed without leaking the provider body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategoryView {
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

impl From<agent_core::provider::ProviderErrorCategory> for ProviderErrorCategoryView {
    fn from(category: agent_core::provider::ProviderErrorCategory) -> Self {
        use agent_core::provider::ProviderErrorCategory;

        match category {
            ProviderErrorCategory::ContextOverflow => Self::ContextOverflow,
            ProviderErrorCategory::Quota => Self::Quota,
            ProviderErrorCategory::UsageNotIncluded => Self::UsageNotIncluded,
            ProviderErrorCategory::CyberPolicy => Self::CyberPolicy,
            ProviderErrorCategory::InvalidPrompt => Self::InvalidPrompt,
            ProviderErrorCategory::InvalidImage => Self::InvalidImage,
            ProviderErrorCategory::RateLimited => Self::RateLimited,
            ProviderErrorCategory::Overloaded => Self::Overloaded,
            ProviderErrorCategory::Authentication => Self::Authentication,
            ProviderErrorCategory::PermissionDenied => Self::PermissionDenied,
            ProviderErrorCategory::Incomplete => Self::Incomplete,
            ProviderErrorCategory::InvalidRequest => Self::InvalidRequest,
            ProviderErrorCategory::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnMetadataNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub metadata: Box<ResponseMetadataView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEventNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub extension: ProviderExtensionView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResponseItemNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub phase: ResponseItemPhaseView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_index: Option<u64>,
    pub item: Box<ResponseItemView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ResponseItemPhaseView {
    Added,
    Done,
}

impl From<agent_core::provider::ResponseItemPhase> for ResponseItemPhaseView {
    fn from(phase: agent_core::provider::ResponseItemPhase) -> Self {
        match phase {
            agent_core::provider::ResponseItemPhase::Added => Self::Added,
            agent_core::provider::ResponseItemPhase::Done => Self::Done,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResponseItemView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub kind: ResponseItemKindView,
    pub payload: ProviderExtensionView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResponseItemKindView {
    Message,
    AgentMessage,
    Reasoning,
    LocalShellCall,
    FunctionCall,
    FunctionCallOutput,
    CustomToolCall,
    CustomToolCallOutput,
    ToolSearchCall,
    ToolSearchOutput,
    WebSearchCall,
    ImageGenerationCall,
    Compaction,
    CompactionTrigger,
    ContextCompaction,
    Other { wire_type: String },
}

impl From<&agent_core::provider::ResponseItemKind> for ResponseItemKindView {
    fn from(kind: &agent_core::provider::ResponseItemKind) -> Self {
        use agent_core::provider::ResponseItemKind as Kind;
        match kind {
            Kind::Message => Self::Message,
            Kind::AgentMessage => Self::AgentMessage,
            Kind::Reasoning => Self::Reasoning,
            Kind::LocalShellCall => Self::LocalShellCall,
            Kind::FunctionCall => Self::FunctionCall,
            Kind::FunctionCallOutput => Self::FunctionCallOutput,
            Kind::CustomToolCall => Self::CustomToolCall,
            Kind::CustomToolCallOutput => Self::CustomToolCallOutput,
            Kind::ToolSearchCall => Self::ToolSearchCall,
            Kind::ToolSearchOutput => Self::ToolSearchOutput,
            Kind::WebSearchCall => Self::WebSearchCall,
            Kind::ImageGenerationCall => Self::ImageGenerationCall,
            Kind::Compaction => Self::Compaction,
            Kind::CompactionTrigger => Self::CompactionTrigger,
            Kind::ContextCompaction => Self::ContextCompaction,
            Kind::Other(wire_type) => Self::Other {
                wire_type: wire_type.clone(),
            },
        }
    }
}

impl From<&agent_core::provider::ResponseItem> for ResponseItemView {
    fn from(item: &agent_core::provider::ResponseItem) -> Self {
        Self {
            id: item.id().map(str::to_string),
            status: item.status().map(str::to_string),
            kind: ResponseItemKindView::from(item.kind()),
            payload: ProviderExtensionView::from(item.payload()),
            diagnostic: item.diagnostic().map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderExtensionView {
    pub event_type: String,
    pub payload: Value,
    pub original_bytes: u64,
    pub truncated: bool,
    pub redacted: bool,
}

impl From<&agent_core::provider::ProviderExtension> for ProviderExtensionView {
    fn from(extension: &agent_core::provider::ProviderExtension) -> Self {
        Self {
            event_type: extension.event_type().to_string(),
            payload: extension.payload().clone(),
            original_bytes: extension.original_bytes(),
            truncated: extension.is_truncated(),
            redacted: extension.was_redacted(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SafetyMetadataView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_cases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_model: Option<String>,
}

impl SafetyMetadataView {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningMetadataView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_included: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl ReasoningMetadataView {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResponseMetadataView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "SafetyMetadataView::is_empty")]
    pub safety: SafetyMetadataView,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifications: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<ProviderExtensionView>,
    #[serde(default, skip_serializing_if = "ReasoningMetadataView::is_empty")]
    pub reasoning: ReasoningMetadataView,
}

impl From<&agent_core::provider::ResponseMetadata> for ResponseMetadataView {
    fn from(metadata: &agent_core::provider::ResponseMetadata) -> Self {
        Self {
            response_id: metadata.response_id.clone(),
            model: metadata.model.clone(),
            service_tier: metadata.service_tier.clone(),
            request_id: metadata.request_id.clone(),
            turn_state: metadata.turn_state.clone(),
            models_etag: metadata.models_etag.clone(),
            end_turn: metadata.end_turn,
            safety: SafetyMetadataView {
                use_cases: metadata.safety.use_cases.clone(),
                reasons: metadata.safety.reasons.clone(),
                retry_model: metadata.safety.retry_model.clone(),
            },
            verifications: metadata.verifications.clone(),
            moderation: metadata.moderation.as_ref().map(Into::into),
            reasoning: ReasoningMetadataView {
                server_included: metadata.reasoning.server_included,
                item_id: metadata.reasoning.item_id.clone(),
                status: metadata.reasoning.status.clone(),
            },
        }
    }
}
