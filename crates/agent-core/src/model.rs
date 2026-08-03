//! Canonical model descriptor and the immutable runtime resolved for one turn.
//!
//! Adapters own discovery and precedence. The core owns the effective contract
//! so prompt construction, budgeting and provider serialization cannot derive
//! independent answers from a model slug.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::tool_spec::{ToolKind, ToolSpec};

pub const MAX_MODEL_INSTRUCTIONS_BYTES: usize = 64 * 1024;

/// Is this the hexadecimal SHA-256 every fingerprint in the system is? One
/// definition, because a model runtime and a context baseline must agree on
/// what a valid fingerprint looks like.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelRuntimeSource {
    Remote {
        endpoint: String,
        fetched_at: String,
    },
    Embedded {
        version: String,
    },
    /// Model descriptor supplied explicitly by provider configuration.
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesDialect {
    Standard,
    Lite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelToolMode {
    /// Every tool is called directly by the model.
    Direct,
    /// The model may call tools directly AND orchestrate them from a cell.
    CodeMode,
    /// The model orchestrates only: the nested tools stay dispatchable but
    /// invisible, and `exec`/`wait` are the model-facing surface.
    CodeModeOnly,
}

impl ModelToolMode {
    /// Does this mode need a Code Mode runtime to be usable at all?
    pub fn needs_code_mode(self) -> bool {
        matches!(self, Self::CodeMode | Self::CodeModeOnly)
    }

    /// Are the nested tools hidden from the model?
    pub fn hides_nested_tools(self) -> bool {
        matches!(self, Self::CodeModeOnly)
    }
}

/// Multi-agent protocol a model was trained to drive.
///
/// The three values are the baseline's own (`multi_agent_versions` of the
/// parity matrix). `Disabled` is what an entry omitting the field means, which
/// is how Codex reads it and why it is the `Default`: a model that says nothing
/// about orchestration must never be handed orchestration tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiAgentVersion {
    #[default]
    Disabled,
    V1,
    V2,
}

impl MultiAgentVersion {
    /// Does this model drive the v2 surface Pyxis implements?
    ///
    /// `V1` deliberately answers `false`: the baseline v1 surface is a
    /// different tool set (`send_input`, `close_agent`, `resume_agent`), and
    /// exposing v2 tools to a v1 model would be a contract Pyxis invented.
    pub fn drives_v2(self) -> bool {
        matches!(self, Self::V2)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

impl std::fmt::Display for MultiAgentVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationMode {
    Bytes,
    Tokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationPolicy {
    pub mode: TruncationMode,
    pub limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelRetryPolicy {
    /// Total provider openings allowed for one sampling, including the initial
    /// request and reopenings after refresh, replay downgrade or model fallback.
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
}

impl<'de> Deserialize<'de> for ModelRetryPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            max_attempts: Option<u32>,
            max_retries: Option<u32>,
            backoff_base_ms: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        let max_attempts = wire
            .max_attempts
            .or_else(|| wire.max_retries.map(|retries| retries.saturating_add(1)))
            .ok_or_else(|| serde::de::Error::missing_field("max_attempts"))?;

        Ok(Self {
            max_attempts,
            backoff_base_ms: wire.backoff_base_ms,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplaySupport {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchToolType {
    #[default]
    Text,
    TextAndImage,
}

/// Model-specific tool contract resolved from the catalog.
///
/// Provider capabilities answer whether the adapter can encode a tool. These
/// fields answer the independent question of whether the selected model was
/// declared capable of using it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelToolCapabilities {
    #[serde(default)]
    pub supports_search_tool: bool,
    #[serde(default)]
    pub web_search_tool_type: WebSearchToolType,
    #[serde(default)]
    pub experimental_supported_tools: Vec<String>,
}

impl ModelToolCapabilities {
    pub fn ensure_tools_supported(
        &self,
        tools: &[ToolSpec],
    ) -> Result<(), ModelToolCompatibilityError> {
        for tool in tools {
            self.ensure_tool_supported(tool)?;
        }
        Ok(())
    }

    fn ensure_tool_supported(&self, tool: &ToolSpec) -> Result<(), ModelToolCompatibilityError> {
        match &tool.kind {
            ToolKind::Namespace { tools } => self.ensure_tools_supported(tools),
            ToolKind::ToolSearch { .. } if !self.supports_search_tool => {
                Err(ModelToolCompatibilityError {
                    tool: tool.name.clone(),
                    reason: "selected model does not support tool search".into(),
                })
            }
            ToolKind::WebSearch { content_types, .. }
                if content_types.iter().any(|kind| kind == "image")
                    && self.web_search_tool_type != WebSearchToolType::TextAndImage =>
            {
                Err(ModelToolCompatibilityError {
                    tool: tool.name.clone(),
                    reason: "selected model supports text-only web search".into(),
                })
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tool {tool} is unsupported by the selected model: {reason}")]
pub struct ModelToolCompatibilityError {
    pub tool: String,
    pub reason: String,
}

/// One complete catalog entry before session configuration is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub slug: String,
    pub display_name: String,
    pub instructions: String,
    pub context_window: u32,
    pub auto_compact_token_limit: u32,
    pub input_modalities: Vec<InputModality>,
    pub supports_reasoning: bool,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub supports_verbosity: bool,
    pub default_verbosity: Option<String>,
    pub supports_parallel_tool_calls: bool,
    #[serde(default)]
    pub tool_capabilities: ModelToolCapabilities,
    #[serde(default)]
    pub service_tiers: Vec<String>,
    #[serde(default)]
    pub reasoning_replay: ReasoningReplaySupport,
    pub responses_dialect: ResponsesDialect,
    pub tool_mode: ModelToolMode,
    /// Orchestration protocol the model drives. Defaulted so a catalog entry
    /// (and a log line) written before the field existed still loads.
    #[serde(default)]
    pub multi_agent_version: MultiAgentVersion,
    pub truncation: TruncationPolicy,
    pub comp_hash: Option<String>,
}

/// The rules a model contract must satisfy, whichever side it comes from: a
/// catalog entry or the runtime resolved for one turn.
///
/// Borrowed rather than owned. The runtime used to validate itself by BUILDING
/// a `ModelDescriptor`, which meant inventing a `display_name` and a list of
/// supported efforts it does not have; every field added to the descriptor was
/// one more value to fabricate here.
struct ModelContract<'a> {
    slug: &'a str,
    instructions: &'a str,
    context_window: u32,
    auto_compact_token_limit: u32,
    input_modalities: &'a [InputModality],
    supports_verbosity: bool,
    verbosity: Option<&'a str>,
    tool_capabilities: &'a ModelToolCapabilities,
    service_tiers: &'a [String],
    truncation: TruncationPolicy,
    comp_hash: Option<&'a str>,
}

impl ModelContract<'_> {
    fn validate(&self) -> Result<(), ModelRuntimeError> {
        if self.slug.trim().is_empty() {
            return Err(ModelRuntimeError::EmptySlug);
        }
        bounded("slug", self.slug, 256)?;
        if self.instructions.trim().is_empty() {
            return Err(ModelRuntimeError::InvalidField {
                field: "instructions",
                detail: "must not be empty".into(),
            });
        }
        if self.instructions.len() > MAX_MODEL_INSTRUCTIONS_BYTES {
            return Err(ModelRuntimeError::InstructionsTooLarge {
                bytes: self.instructions.len(),
                max: MAX_MODEL_INSTRUCTIONS_BYTES,
            });
        }
        if self.context_window == 0 {
            return Err(ModelRuntimeError::InvalidField {
                field: "context_window",
                detail: "must be greater than zero".into(),
            });
        }
        if self.auto_compact_token_limit == 0 || self.auto_compact_token_limit > self.context_window
        {
            return Err(ModelRuntimeError::InvalidField {
                field: "auto_compact_token_limit",
                detail: "must be within the context window".into(),
            });
        }
        if !self.input_modalities.contains(&InputModality::Text) {
            return Err(ModelRuntimeError::InvalidField {
                field: "input_modalities",
                detail: "text is required".into(),
            });
        }
        if self.input_modalities.len() > 8 {
            return Err(ModelRuntimeError::InvalidField {
                field: "input_modalities",
                detail: "must not contain more than 8 entries".into(),
            });
        }
        if !self.supports_verbosity && self.verbosity.is_some() {
            return Err(ModelRuntimeError::InvalidField {
                field: "default_verbosity",
                detail: "requires verbosity support".into(),
            });
        }
        if let Some(verbosity) = self.verbosity {
            bounded("default_verbosity", verbosity, 64)?;
        }
        unique_bounded(
            "tool_capabilities.experimental_supported_tools",
            &self.tool_capabilities.experimental_supported_tools,
            64,
            128,
        )?;
        unique_bounded("service_tiers", self.service_tiers, 32, 64)?;
        let minimum_feedback = match self.truncation.mode {
            TruncationMode::Bytes => crate::tools::MIN_MODEL_TOOL_RESULT_BYTES,
            TruncationMode::Tokens => crate::tools::MIN_MODEL_TOOL_RESULT_TOKENS,
        };
        if usize::try_from(self.truncation.limit).unwrap_or(usize::MAX) < minimum_feedback {
            return Err(ModelRuntimeError::InvalidField {
                field: "truncation.limit",
                detail: format!("must be at least {minimum_feedback} for terminal tool feedback"),
            });
        }
        if let Some(hash) = self.comp_hash {
            bounded("comp_hash", hash, 256)?;
        }
        Ok(())
    }
}

/// Non-empty, at most `max` bytes, no control characters. The shape every
/// short text field of a model contract has to have.
fn bounded(field: &'static str, value: &str, max: usize) -> Result<(), ModelRuntimeError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(ModelRuntimeError::InvalidField {
            field,
            detail: format!("must contain 1 to {max} bytes and no control characters"),
        });
    }
    Ok(())
}

/// Same, for a list whose entries must also be distinct (case-insensitively).
fn unique_bounded(
    field: &'static str,
    values: &[String],
    max_count: usize,
    max_len: usize,
) -> Result<(), ModelRuntimeError> {
    let unique: HashSet<String> = values
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    if values.len() > max_count || unique.len() != values.len() {
        return Err(ModelRuntimeError::InvalidField {
            field,
            detail: format!("must contain at most {max_count} unique values"),
        });
    }
    for value in values {
        bounded(field, value, max_len)?;
    }
    Ok(())
}

impl ModelDescriptor {
    fn contract(&self) -> ModelContract<'_> {
        ModelContract {
            slug: &self.slug,
            instructions: &self.instructions,
            context_window: self.context_window,
            auto_compact_token_limit: self.auto_compact_token_limit,
            input_modalities: &self.input_modalities,
            supports_verbosity: self.supports_verbosity,
            verbosity: self.default_verbosity.as_deref(),
            tool_capabilities: &self.tool_capabilities,
            service_tiers: &self.service_tiers,
            truncation: self.truncation,
            comp_hash: self.comp_hash.as_deref(),
        }
    }

    pub fn validate(&self) -> Result<(), ModelRuntimeError> {
        self.contract().validate()?;
        if self.display_name.len() > 512 || self.display_name.chars().any(char::is_control) {
            return Err(ModelRuntimeError::InvalidField {
                field: "display_name",
                detail: "must not exceed 512 bytes or contain control characters".into(),
            });
        }
        // Catalog-only: an entry declares the WHOLE menu of efforts, where a
        // resolved runtime carries the single one that was selected.
        if self.supports_reasoning && self.supported_reasoning_efforts.is_empty() {
            return Err(ModelRuntimeError::InvalidField {
                field: "supported_reasoning_efforts",
                detail: "must not be empty when reasoning is supported".into(),
            });
        }
        if !self.supports_reasoning
            && (self.default_reasoning_effort.is_some()
                || !self.supported_reasoning_efforts.is_empty())
        {
            return Err(ModelRuntimeError::InvalidField {
                field: "supports_reasoning",
                detail: "contradicts the declared reasoning efforts".into(),
            });
        }
        if self.supported_reasoning_efforts.len() > 32 {
            return Err(ModelRuntimeError::InvalidField {
                field: "supported_reasoning_efforts",
                detail: "must not contain more than 32 efforts".into(),
            });
        }
        for effort in &self.supported_reasoning_efforts {
            bounded("supported_reasoning_efforts", effort, 64)?;
        }
        if let Some(default) = self.default_reasoning_effort.as_deref()
            && !self
                .supported_reasoning_efforts
                .iter()
                .any(|effort| effort.eq_ignore_ascii_case(default))
        {
            return Err(ModelRuntimeError::InvalidField {
                field: "default_reasoning_effort",
                detail: "must be one of the supported efforts".into(),
            });
        }
        Ok(())
    }
}

/// The only model contract consumed during a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModelRuntime {
    pub slug: String,
    pub source: ModelRuntimeSource,
    pub instructions: String,
    pub fingerprint: String,
    pub context_window: u32,
    pub auto_compact_token_limit: u32,
    pub input_modalities: Vec<InputModality>,
    pub reasoning_effort: Option<String>,
    pub supports_verbosity: bool,
    pub verbosity: Option<String>,
    pub supports_parallel_tool_calls: bool,
    #[serde(default)]
    pub tool_capabilities: ModelToolCapabilities,
    #[serde(default)]
    pub service_tiers: Vec<String>,
    #[serde(default)]
    pub reasoning_replay: ReasoningReplaySupport,
    pub responses_dialect: ResponsesDialect,
    pub tool_mode: ModelToolMode,
    #[serde(default)]
    pub multi_agent_version: MultiAgentVersion,
    pub truncation: TruncationPolicy,
    pub retry: ModelRetryPolicy,
    pub max_output_tokens: u32,
    pub comp_hash: Option<String>,
}

impl ResolvedModelRuntime {
    pub fn validate(&self) -> Result<(), ModelRuntimeError> {
        ModelContract {
            slug: &self.slug,
            instructions: &self.instructions,
            context_window: self.context_window,
            auto_compact_token_limit: self.auto_compact_token_limit,
            input_modalities: &self.input_modalities,
            supports_verbosity: self.supports_verbosity,
            verbosity: self.verbosity.as_deref(),
            tool_capabilities: &self.tool_capabilities,
            service_tiers: &self.service_tiers,
            truncation: self.truncation,
            comp_hash: self.comp_hash.as_deref(),
        }
        .validate()?;
        // The turn carries ONE effort, already selected from the catalog menu.
        if let Some(effort) = self.reasoning_effort.as_deref() {
            bounded("reasoning_effort", effort, 64)?;
        }
        if !is_sha256(&self.fingerprint) {
            return Err(ModelRuntimeError::InvalidField {
                field: "fingerprint",
                detail: "must be a 64-character hexadecimal SHA-256".into(),
            });
        }
        if self.max_output_tokens == 0 || self.max_output_tokens >= self.context_window {
            return Err(ModelRuntimeError::InvalidField {
                field: "max_output_tokens",
                detail: "must be greater than zero and below the context window".into(),
            });
        }
        if self.retry.max_attempts == 0 {
            return Err(ModelRuntimeError::InvalidField {
                field: "retry.max_attempts",
                detail: "must be greater than zero".into(),
            });
        }
        if self.retry.backoff_base_ms == 0 {
            return Err(ModelRuntimeError::InvalidField {
                field: "retry.backoff_base_ms",
                detail: "must be greater than zero".into(),
            });
        }
        if self.reasoning_replay == ReasoningReplaySupport::Enabled
            && self.reasoning_effort.is_none()
        {
            return Err(ModelRuntimeError::InvalidField {
                field: "reasoning_replay",
                detail: "requires an active reasoning effort".into(),
            });
        }
        Ok(())
    }

    pub fn accepts_images(&self) -> bool {
        self.input_modalities.contains(&InputModality::Image)
    }

    pub fn accepts_audio(&self) -> bool {
        self.input_modalities.contains(&InputModality::Audio)
    }

    pub fn ensure_tools_supported(
        &self,
        tools: &[ToolSpec],
    ) -> Result<(), ModelToolCompatibilityError> {
        self.tool_capabilities.ensure_tools_supported(tools)
    }

    /// The baseline's `use_responses_lite`, read back from the dialect it was
    /// resolved into. Kept as a named question so a caller never has to know
    /// that "lite" and "standard" are what the flag became.
    pub fn uses_responses_lite(&self) -> bool {
        self.responses_dialect == ResponsesDialect::Lite
    }

    pub fn reasoning_replay_disabled_reason(&self) -> Option<&'static str> {
        (self.reasoning_replay == ReasoningReplaySupport::Disabled)
            .then_some("descriptor does not prove encrypted stateless reasoning replay")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelRuntimeError {
    #[error("model slug is empty")]
    EmptySlug,
    #[error("model instructions exceed {max} bytes ({bytes})")]
    InstructionsTooLarge { bytes: usize, max: usize },
    #[error("invalid model field {field}: {detail}")]
    InvalidField { field: &'static str, detail: String },
    #[error("model {slug} is incompatible: {reason}")]
    Incompatible { slug: String, reason: String },
    #[error("model {slug} does not support reasoning effort {effort}")]
    UnsupportedReasoningEffort { slug: String, effort: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_instructions_fail_before_provider_use() {
        let descriptor = ModelDescriptor {
            slug: "model".into(),
            display_name: "Model".into(),
            instructions: "x".repeat(MAX_MODEL_INSTRUCTIONS_BYTES + 1),
            context_window: 10_000,
            auto_compact_token_limit: 8_000,
            input_modalities: vec![InputModality::Text],
            supports_reasoning: false,
            default_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
            supports_verbosity: false,
            default_verbosity: None,
            supports_parallel_tool_calls: false,
            tool_capabilities: ModelToolCapabilities::default(),
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
        };
        assert!(matches!(
            descriptor.validate(),
            Err(ModelRuntimeError::InstructionsTooLarge { .. })
        ));
    }

    #[test]
    fn undersized_tool_feedback_budget_is_rejected() {
        let mut descriptor = ModelDescriptor {
            slug: "model".into(),
            display_name: "Model".into(),
            instructions: "instructions".into(),
            context_window: 10_000,
            auto_compact_token_limit: 8_000,
            input_modalities: vec![InputModality::Text],
            supports_reasoning: false,
            default_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
            supports_verbosity: false,
            default_verbosity: None,
            supports_parallel_tool_calls: false,
            tool_capabilities: ModelToolCapabilities::default(),
            service_tiers: Vec::new(),
            reasoning_replay: ReasoningReplaySupport::Disabled,
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
            multi_agent_version: MultiAgentVersion::Disabled,
            truncation: TruncationPolicy {
                mode: TruncationMode::Tokens,
                limit: 1,
            },
            comp_hash: None,
        };
        assert!(matches!(
            descriptor.validate(),
            Err(ModelRuntimeError::InvalidField {
                field: "truncation.limit",
                ..
            })
        ));
        descriptor.truncation.limit = crate::tools::MIN_MODEL_TOOL_RESULT_TOKENS as u32;
        assert!(descriptor.validate().is_ok());
    }

    /// US-010: a descriptor written before the field existed loads as
    /// `disabled`, and the three baseline values round-trip under their wire
    /// spelling. A silent `v2` on a legacy entry would hand orchestration tools
    /// to a model that never asked for them.
    #[test]
    fn an_absent_multi_agent_version_reads_as_disabled_and_the_wire_names_round_trip() {
        let descriptor = ModelDescriptor {
            slug: "model".into(),
            display_name: "Model".into(),
            instructions: "instructions".into(),
            context_window: 10_000,
            auto_compact_token_limit: 8_000,
            input_modalities: vec![InputModality::Text],
            supports_reasoning: false,
            default_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
            supports_verbosity: false,
            default_verbosity: None,
            supports_parallel_tool_calls: false,
            tool_capabilities: ModelToolCapabilities::default(),
            service_tiers: Vec::new(),
            reasoning_replay: ReasoningReplaySupport::Disabled,
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
            multi_agent_version: MultiAgentVersion::Disabled,
            truncation: TruncationPolicy {
                mode: TruncationMode::Tokens,
                limit: crate::tools::MIN_MODEL_TOOL_RESULT_TOKENS as u32,
            },
            comp_hash: None,
        };
        let mut wire = serde_json::to_value(&descriptor).unwrap();
        wire.as_object_mut().unwrap().remove("multi_agent_version");
        let legacy: ModelDescriptor = serde_json::from_value(wire).unwrap();
        assert_eq!(legacy.multi_agent_version, MultiAgentVersion::Disabled);
        assert!(!legacy.multi_agent_version.drives_v2());

        for (version, wire) in [
            (MultiAgentVersion::Disabled, "disabled"),
            (MultiAgentVersion::V1, "v1"),
            (MultiAgentVersion::V2, "v2"),
        ] {
            assert_eq!(serde_json::to_value(version).unwrap(), wire);
            assert_eq!(version.as_str(), wire);
        }
        // Only v2 is a surface Pyxis implements.
        assert!(MultiAgentVersion::V2.drives_v2());
        assert!(!MultiAgentVersion::V1.drives_v2());
    }

    #[test]
    fn legacy_retry_count_deserializes_as_total_attempts() {
        let retry: ModelRetryPolicy = serde_json::from_value(serde_json::json!({
            "max_retries": 3,
            "backoff_base_ms": 50
        }))
        .unwrap();

        assert_eq!(retry.max_attempts, 4);
        assert_eq!(
            serde_json::to_value(retry).unwrap(),
            serde_json::json!({
                "max_attempts": 4,
                "backoff_base_ms": 50
            })
        );
    }

    #[test]
    fn model_tool_capabilities_reject_only_the_unsupported_model_surface() {
        let search = ToolSpec::tool_search(
            "find tools",
            "client",
            serde_json::json!({"type":"object","properties":{}}),
        );
        let web_with_images = ToolSpec {
            name: "web_search".into(),
            description: String::new(),
            kind: ToolKind::WebSearch {
                external_web_access: Some(true),
                indexed_web_access: None,
                filters: None,
                location: None,
                context_size: None,
                content_types: vec!["text".into(), "image".into()],
            },
        };
        let text_only = ModelToolCapabilities::default();
        assert!(matches!(
            text_only.ensure_tools_supported(std::slice::from_ref(&search)),
            Err(ModelToolCompatibilityError { tool, .. }) if tool == "tool_search"
        ));
        assert!(
            text_only
                .ensure_tools_supported(std::slice::from_ref(&web_with_images))
                .is_err()
        );

        let complete = ModelToolCapabilities {
            supports_search_tool: true,
            web_search_tool_type: WebSearchToolType::TextAndImage,
            experimental_supported_tools: vec!["tool_search".into()],
        };
        complete
            .ensure_tools_supported(&[search, web_with_images])
            .unwrap();
    }
}
