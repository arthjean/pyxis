use agent_auth::ProviderId;
use agent_core::model::ModelDescriptor;
use agent_core::provider::{
    AuxiliaryCapabilities, CacheCapabilities, Capabilities, CapabilityLimits, ProviderError,
    ReasoningCapabilities, ToolCallingCapabilities,
};

use crate::chatgpt_error::invalid_request;
use crate::chatgpt_http::ResponsesTransportConfig;
use crate::models::ModelCatalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiEndpointKind {
    Standard,
    AzureResponses,
}

impl OpenAiEndpointKind {
    pub(super) fn stores_responses(self) -> bool {
        matches!(self, Self::AzureResponses)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenAiAuthPolicy {
    #[default]
    Required,
    AllowUnauthenticated,
}

#[derive(Debug, Clone)]
pub enum OpenAiCatalogPolicy {
    Static(Vec<ModelDescriptor>),
    Remote { models_path: String },
}

#[derive(Clone)]
pub struct ConfiguredOpenAiConfig {
    pub(super) name: String,
    pub(super) auth_provider: ProviderId,
    pub(super) endpoint_kind: OpenAiEndpointKind,
    pub(super) transport: ResponsesTransportConfig,
    pub(super) catalog: OpenAiCatalogPolicy,
    pub(super) capabilities: Capabilities,
    auth_policy: OpenAiAuthPolicy,
}

impl std::fmt::Debug for ConfiguredOpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredOpenAiConfig")
            .field("name", &self.name)
            .field("auth_provider", &self.auth_provider)
            .field("endpoint_kind", &self.endpoint_kind)
            .field("transport", &self.transport)
            .field("catalog", &self.catalog)
            .field("capabilities", &self.capabilities)
            .field("auth_policy", &self.auth_policy)
            .finish()
    }
}

impl ConfiguredOpenAiConfig {
    pub fn new(
        name: impl Into<String>,
        endpoint_kind: OpenAiEndpointKind,
        transport: ResponsesTransportConfig,
        catalog: OpenAiCatalogPolicy,
    ) -> Result<Self, ProviderError> {
        let config = Self {
            name: name.into(),
            auth_provider: ProviderId::OpenAiResponses,
            endpoint_kind,
            transport,
            catalog,
            capabilities: default_capabilities(),
            auth_policy: OpenAiAuthPolicy::Required,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn with_auth_provider(mut self, provider_id: ProviderId) -> Result<Self, ProviderError> {
        self.auth_provider = provider_id;
        self.validate()?;
        Ok(self)
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Result<Self, ProviderError> {
        self.capabilities = capabilities;
        self.validate()?;
        Ok(self)
    }

    pub fn with_auth_policy(mut self, policy: OpenAiAuthPolicy) -> Self {
        self.auth_policy = policy;
        self
    }

    pub fn with_auxiliary_capabilities(mut self, capabilities: AuxiliaryCapabilities) -> Self {
        self.capabilities.auxiliary = capabilities;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn endpoint(&self) -> Result<url::Url, ProviderError> {
        self.transport.endpoint()
    }

    pub(super) fn allow_unauthenticated(&self) -> bool {
        self.auth_policy == OpenAiAuthPolicy::AllowUnauthenticated
    }

    pub(super) fn uses_chatgpt_backend(&self) -> bool {
        self.auth_provider == ProviderId::OpenAiChatGpt
    }

    pub(super) fn validate(&self) -> Result<(), ProviderError> {
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err(invalid_request(
                "invalid provider field `name`: expected 1..=128 printable bytes",
            ));
        }
        if !matches!(
            self.auth_provider,
            ProviderId::OpenAiResponses | ProviderId::OpenAiChatGpt
        ) {
            return Err(invalid_request(
                "invalid provider field `auth_provider`: expected an OpenAI Responses provider",
            ));
        }
        self.transport.validate_for_configured()?;
        self.capabilities.validate().map_err(|error| {
            invalid_request(format!("invalid provider field `capabilities`: {error}"))
        })?;
        match &self.catalog {
            OpenAiCatalogPolicy::Static(models) => {
                ModelCatalog::from_static(models.clone()).map_err(|error| {
                    invalid_request(format!("invalid provider field `models`: {error}"))
                })?;
            }
            OpenAiCatalogPolicy::Remote { models_path } => {
                self.transport.endpoint_for_path(models_path)?;
            }
        }
        Ok(())
    }
}

fn default_capabilities() -> Capabilities {
    Capabilities {
        vision: true,
        tools: true,
        structured_output: true,
        prompt_caching: true,
        reasoning: true,
        server_side_state: false,
        max_context: 256_000,
        limits: CapabilityLimits {
            max_images_per_request: None,
            max_tool_schema_bytes: Some(64 * 1024),
        },
        tool_calling: ToolCallingCapabilities {
            parallel_tool_calls: true,
            strict_json_schema: true,
            freeform_tools: true,
            namespace_tools: true,
            tool_search: true,
            web_search: true,
        },
        reasoning_options: ReasoningCapabilities {
            encrypted_replay: true,
        },
        cache: CacheCapabilities {
            prompt_cache_key: true,
        },
        auxiliary: AuxiliaryCapabilities::default(),
    }
}
