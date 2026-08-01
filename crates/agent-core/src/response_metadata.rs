//! Provider-neutral metadata observed around one model response.

use serde::{Deserialize, Serialize};

use crate::provider_extension::ProviderExtension;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SafetyMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_cases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_model: Option<String>,
}

impl SafetyMetadata {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_included: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl ReasoningMetadata {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResponseMetadata {
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
    #[serde(default, skip_serializing_if = "SafetyMetadata::is_empty")]
    pub safety: SafetyMetadata,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifications: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<ProviderExtension>,
    #[serde(default, skip_serializing_if = "ReasoningMetadata::is_empty")]
    pub reasoning: ReasoningMetadata,
}

impl ResponseMetadata {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}
