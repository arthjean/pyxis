//! External projection of provider-neutral response metadata.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
