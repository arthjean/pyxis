//! Canonical model descriptor and the immutable runtime resolved for one turn.
//!
//! Adapters own discovery and precedence. The core owns the effective contract
//! so prompt construction, budgeting and provider serialization cannot derive
//! independent answers from a model slug.

use serde::{Deserialize, Serialize};

pub const MAX_MODEL_INSTRUCTIONS_BYTES: usize = 64 * 1024;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
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
    Direct,
    CodeModeOnly,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRetryPolicy {
    pub max_retries: u32,
    pub backoff_base_ms: u64,
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
    pub responses_dialect: ResponsesDialect,
    pub tool_mode: ModelToolMode,
    pub truncation: TruncationPolicy,
    pub comp_hash: Option<String>,
}

impl ModelDescriptor {
    pub fn validate(&self) -> Result<(), ModelRuntimeError> {
        if self.slug.trim().is_empty() {
            return Err(ModelRuntimeError::EmptySlug);
        }
        if self.slug.len() > 256 || self.slug.chars().any(char::is_control) {
            return Err(ModelRuntimeError::InvalidField {
                field: "slug",
                detail: "must not exceed 256 bytes or contain control characters".into(),
            });
        }
        if self.display_name.len() > 512 || self.display_name.chars().any(char::is_control) {
            return Err(ModelRuntimeError::InvalidField {
                field: "display_name",
                detail: "must not exceed 512 bytes or contain control characters".into(),
            });
        }
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
        if self.supports_reasoning && self.supported_reasoning_efforts.is_empty() {
            return Err(ModelRuntimeError::InvalidField {
                field: "supported_reasoning_efforts",
                detail: "must not be empty when reasoning is supported".into(),
            });
        }
        if self.supported_reasoning_efforts.iter().any(|effort| {
            effort.trim().is_empty() || effort.len() > 64 || effort.chars().any(char::is_control)
        }) {
            return Err(ModelRuntimeError::InvalidField {
                field: "supported_reasoning_efforts",
                detail: "must contain non-empty efforts of at most 64 bytes".into(),
            });
        }
        if self.supported_reasoning_efforts.len() > 32 {
            return Err(ModelRuntimeError::InvalidField {
                field: "supported_reasoning_efforts",
                detail: "must not contain more than 32 efforts".into(),
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
        if !self.supports_verbosity && self.default_verbosity.is_some() {
            return Err(ModelRuntimeError::InvalidField {
                field: "default_verbosity",
                detail: "requires verbosity support".into(),
            });
        }
        if self.default_verbosity.as_ref().is_some_and(|verbosity| {
            verbosity.is_empty() || verbosity.len() > 64 || verbosity.chars().any(char::is_control)
        }) {
            return Err(ModelRuntimeError::InvalidField {
                field: "default_verbosity",
                detail: "must contain at most 64 bytes".into(),
            });
        }
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
        if self.comp_hash.as_ref().is_some_and(|hash| {
            hash.is_empty() || hash.len() > 256 || hash.chars().any(char::is_control)
        }) {
            return Err(ModelRuntimeError::InvalidField {
                field: "comp_hash",
                detail: "must contain at most 256 bytes".into(),
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
    pub responses_dialect: ResponsesDialect,
    pub tool_mode: ModelToolMode,
    pub truncation: TruncationPolicy,
    pub retry: ModelRetryPolicy,
    pub max_output_tokens: u32,
    pub comp_hash: Option<String>,
}

impl ResolvedModelRuntime {
    pub fn validate(&self) -> Result<(), ModelRuntimeError> {
        ModelDescriptor {
            slug: self.slug.clone(),
            display_name: self.slug.clone(),
            instructions: self.instructions.clone(),
            context_window: self.context_window,
            auto_compact_token_limit: self.auto_compact_token_limit,
            input_modalities: self.input_modalities.clone(),
            supports_reasoning: self.reasoning_effort.is_some(),
            default_reasoning_effort: self.reasoning_effort.clone(),
            supported_reasoning_efforts: self.reasoning_effort.clone().into_iter().collect(),
            supports_verbosity: self.supports_verbosity,
            default_verbosity: self.verbosity.clone(),
            supports_parallel_tool_calls: self.supports_parallel_tool_calls,
            responses_dialect: self.responses_dialect,
            tool_mode: self.tool_mode,
            truncation: self.truncation,
            comp_hash: self.comp_hash.clone(),
        }
        .validate()?;
        if self.fingerprint.len() != 64
            || !self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
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
        if self.retry.backoff_base_ms == 0 {
            return Err(ModelRuntimeError::InvalidField {
                field: "retry.backoff_base_ms",
                detail: "must be greater than zero".into(),
            });
        }
        Ok(())
    }

    pub fn accepts_images(&self) -> bool {
        self.input_modalities.contains(&InputModality::Image)
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
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
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
            responses_dialect: ResponsesDialect::Standard,
            tool_mode: ModelToolMode::Direct,
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
}
