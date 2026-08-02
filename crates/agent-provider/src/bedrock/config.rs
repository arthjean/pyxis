use std::time::Duration;

use agent_core::model::ModelDescriptor;
use agent_core::provider::{Capabilities, ProviderError};

use crate::models::ModelCatalog;

use super::{invalid, valid_aws_region};

#[derive(Debug, Clone)]
pub struct AmazonBedrockConfig {
    pub(super) region: String,
    pub(super) profile: Option<String>,
    pub(super) models: Vec<ModelDescriptor>,
    pub(super) preferred_models: Vec<String>,
    pub(super) capabilities: Capabilities,
    pub(super) idle_timeout: Duration,
}

impl AmazonBedrockConfig {
    pub fn new(
        region: impl Into<String>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, ProviderError> {
        let config = Self {
            region: region.into(),
            profile: None,
            models,
            preferred_models: Vec::new(),
            capabilities: Capabilities {
                max_context: 256_000,
                ..Capabilities::default()
            },
            idle_timeout: Duration::from_secs(60),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Result<Self, ProviderError> {
        self.profile = Some(profile.into());
        self.validate()?;
        Ok(self)
    }

    pub fn with_preferred_models(
        mut self,
        preferred_models: Vec<String>,
    ) -> Result<Self, ProviderError> {
        self.preferred_models = preferred_models;
        self.validate()?;
        Ok(self)
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Result<Self, ProviderError> {
        self.capabilities = capabilities;
        self.validate()?;
        Ok(self)
    }

    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Result<Self, ProviderError> {
        if idle_timeout.is_zero() {
            return Err(invalid("idle_timeout", "must be nonzero"));
        }
        self.idle_timeout = idle_timeout;
        Ok(self)
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn preferred_models(&self) -> &[String] {
        &self.preferred_models
    }

    pub(super) fn validate(&self) -> Result<(), ProviderError> {
        if !valid_aws_region(&self.region) {
            return Err(invalid("region", "must be an AWS region identifier"));
        }
        if let Some(profile) = self.profile.as_deref() {
            validate_printable("profile", profile, 256)?;
        }
        ModelCatalog::from_static(self.models.clone())
            .map_err(|error| invalid("models", error.to_string()))?;
        for preferred in &self.preferred_models {
            if !self.models.iter().any(|model| model.slug == *preferred) {
                return Err(invalid(
                    "preferred_models",
                    format!("model `{preferred}` is not present in the configured catalog"),
                ));
            }
        }
        self.capabilities
            .validate()
            .map_err(|error| invalid("capabilities", error.to_string()))?;
        if self.capabilities.reasoning || self.capabilities.reasoning_options.encrypted_replay {
            return Err(invalid(
                "capabilities",
                "Bedrock reasoning controls are not encoded by this adapter",
            ));
        }
        if self.capabilities.prompt_caching
            || self.capabilities.cache.prompt_cache_key
            || self.capabilities.server_side_state
        {
            return Err(invalid(
                "capabilities",
                "Bedrock caching and server-side state are not encoded by this adapter",
            ));
        }
        if self.capabilities.tool_calling.freeform_tools
            || self.capabilities.tool_calling.namespace_tools
            || self.capabilities.tool_calling.tool_search
            || self.capabilities.tool_calling.web_search
        {
            return Err(invalid(
                "capabilities",
                "Bedrock Converse accepts function tools only",
            ));
        }
        Ok(())
    }
}

fn validate_printable(field: &'static str, value: &str, max: usize) -> Result<(), ProviderError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(invalid(field, "must be a non-empty printable value"));
    }
    Ok(())
}
