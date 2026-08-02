//! Defensive decoder for the remote model-catalog wire format.

use std::collections::BTreeMap;

use agent_core::model::{
    InputModality, ModelDescriptor, ModelRuntimeSource, ModelToolCapabilities, ModelToolMode,
    MultiAgentVersion, ReasoningReplaySupport, ResponsesDialect, TruncationMode, TruncationPolicy,
    WebSearchToolType,
};
use serde::Deserialize;

use super::{CatalogMetadata, CatalogServiceTier, RemoteEntry};

#[derive(Deserialize)]
pub(super) struct WireCatalog {
    #[serde(default)]
    pub(super) models: Vec<WireModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct WireModel {
    pub(super) slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    pub(super) visibility: Option<String>,
    #[serde(default)]
    pub(super) priority: i32,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Option<Vec<WireReasoningLevel>>,
    #[serde(default)]
    shell_type: Option<String>,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    additional_speed_tiers: Vec<String>,
    #[serde(default)]
    base_instructions: Option<String>,
    #[serde(default)]
    model_messages: Option<WireModelMessages>,
    #[serde(default)]
    support_verbosity: Option<bool>,
    #[serde(default)]
    default_verbosity: Option<String>,
    #[serde(default)]
    truncation_policy: Option<WireTruncationPolicy>,
    #[serde(default)]
    supports_parallel_tool_calls: Option<bool>,
    #[serde(default)]
    service_tiers: Vec<WireServiceTier>,
    #[serde(default)]
    default_service_tier: Option<String>,
    #[serde(default)]
    availability_nux: Option<serde_json::Value>,
    #[serde(default)]
    upgrade: Option<serde_json::Value>,
    #[serde(default)]
    include_skills_usage_instructions: bool,
    #[serde(default = "default_true")]
    supports_reasoning_summary_parameter: bool,
    #[serde(default)]
    default_reasoning_summary: Option<serde_json::Value>,
    #[serde(default)]
    apply_patch_tool_type: Option<serde_json::Value>,
    #[serde(default)]
    web_search_tool_type: Option<serde_json::Value>,
    #[serde(default)]
    supports_encrypted_reasoning: Option<bool>,
    #[serde(default)]
    supports_reasoning_replay: Option<bool>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    max_context_window: Option<u32>,
    #[serde(default)]
    auto_compact_token_limit: Option<u32>,
    #[serde(default)]
    comp_hash: Option<String>,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: u8,
    #[serde(default)]
    experimental_supported_tools: Vec<String>,
    #[serde(default)]
    supports_search_tool: bool,
    #[serde(default)]
    supports_image_detail_original: bool,
    #[serde(default)]
    auto_review_model_override: Option<String>,
    #[serde(default)]
    use_responses_lite: Option<bool>,
    #[serde(default)]
    tool_mode: Option<serde_json::Value>,
    #[serde(default)]
    multi_agent_version: Option<serde_json::Value>,
    #[serde(flatten)]
    extra_fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireReasoningLevel {
    effort: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireServiceTier {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireModelMessages {
    #[serde(default)]
    instructions_template: Option<String>,
    #[serde(default)]
    instructions_variables: Option<WireInstructionsVariables>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireInstructionsVariables {
    #[serde(default)]
    personality_default: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireTruncationPolicy {
    mode: String,
    limit: u32,
}

pub(super) fn descriptor_from_wire(
    model: WireModel,
    metadata: Box<CatalogMetadata>,
    source: ModelRuntimeSource,
) -> Result<RemoteEntry, String> {
    let display_name = model
        .display_name
        .clone()
        .unwrap_or_else(|| model.slug.clone());
    if display_name.len() > 512 || display_name.chars().any(char::is_control) {
        return Ok(RemoteEntry::Incompatible {
            display_name: model.slug,
            metadata,
            reason: "display_name exceeds 512 bytes or contains control characters".into(),
            source,
        });
    }
    if !matches!(
        model.visibility.as_deref(),
        None | Some("list" | "hide" | "none")
    ) {
        return Ok(RemoteEntry::Incompatible {
            display_name,
            metadata,
            reason: "unknown required model visibility".into(),
            source,
        });
    }
    if !(1..=100).contains(&model.effective_context_window_percent) {
        return Ok(RemoteEntry::Incompatible {
            display_name,
            metadata,
            reason: "effective_context_window_percent must be between 1 and 100".into(),
            source,
        });
    }
    let tool_mode = match model.tool_mode {
        None | Some(serde_json::Value::Null) => ModelToolMode::Direct,
        Some(serde_json::Value::String(mode)) if mode == "direct" => ModelToolMode::Direct,
        Some(serde_json::Value::String(mode)) if mode == "code_mode" => ModelToolMode::CodeMode,
        Some(serde_json::Value::String(mode)) if mode == "code_mode_only" => {
            ModelToolMode::CodeModeOnly
        }
        Some(value) => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                metadata,
                reason: format!(
                    "unknown required tool_mode {}",
                    bounded_wire_text(&value.to_string(), 128)
                ),
                source,
            });
        }
    };
    let multi_agent_version = match model.multi_agent_version {
        None | Some(serde_json::Value::Null) => MultiAgentVersion::Disabled,
        Some(serde_json::Value::String(ref version)) if version == "disabled" => {
            MultiAgentVersion::Disabled
        }
        Some(serde_json::Value::String(ref version)) if version == "v1" => MultiAgentVersion::V1,
        Some(serde_json::Value::String(ref version)) if version == "v2" => MultiAgentVersion::V2,
        Some(value) => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                metadata,
                reason: format!(
                    "unknown required multi_agent_version {}",
                    bounded_wire_text(&value.to_string(), 128)
                ),
                source,
            });
        }
    };
    let templated_instructions = model.model_messages.and_then(|messages| {
        let personality = messages
            .instructions_variables
            .and_then(|variables| variables.personality_default)
            .unwrap_or_default();
        messages
            .instructions_template
            .map(|template| template.replace("{{ personality }}", &personality))
    });
    let instructions = templated_instructions
        .filter(|instructions| !instructions.is_empty())
        .or_else(|| {
            model
                .base_instructions
                .filter(|instructions| !instructions.is_empty())
        })
        .ok_or_else(|| "missing base instructions".to_string())?;
    let context_window = model
        .context_window
        .or(model.max_context_window)
        .filter(|window| *window > 0)
        .ok_or_else(|| "missing positive context_window".to_string())?;
    let auto_compact_token_limit = model
        .auto_compact_token_limit
        .unwrap_or_else(|| context_window.saturating_mul(9) / 10)
        .min(context_window.saturating_mul(9) / 10);
    let raw_modalities = model
        .input_modalities
        .ok_or_else(|| "missing input_modalities".to_string())?;
    let mut modalities = Vec::with_capacity(raw_modalities.len());
    for modality in raw_modalities {
        match modality.as_str() {
            "text" => modalities.push(InputModality::Text),
            "image" => modalities.push(InputModality::Image),
            "audio" => modalities.push(InputModality::Audio),
            other => {
                return Ok(RemoteEntry::Incompatible {
                    display_name,
                    metadata,
                    reason: format!(
                        "unknown required input modality {}",
                        bounded_wire_text(other, 128)
                    ),
                    source,
                });
            }
        }
    }
    let reasoning = model
        .supported_reasoning_levels
        .ok_or_else(|| "missing supported_reasoning_levels".to_string())?
        .into_iter()
        .map(|level| level.effort)
        .collect::<Vec<_>>();
    let supports_verbosity = model
        .support_verbosity
        .ok_or_else(|| "missing support_verbosity".to_string())?;
    let parallel = model
        .supports_parallel_tool_calls
        .ok_or_else(|| "missing supports_parallel_tool_calls".to_string())?;
    let web_search_tool_type = match model.web_search_tool_type.as_ref() {
        None | Some(serde_json::Value::Null) => WebSearchToolType::Text,
        Some(serde_json::Value::String(kind)) if kind == "text" => WebSearchToolType::Text,
        Some(serde_json::Value::String(kind)) if kind == "text_and_image" => {
            WebSearchToolType::TextAndImage
        }
        Some(value) => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                metadata,
                reason: format!(
                    "unknown required web_search_tool_type {}",
                    bounded_wire_text(&value.to_string(), 128)
                ),
                source,
            });
        }
    };
    let tool_capabilities = ModelToolCapabilities {
        supports_search_tool: model.supports_search_tool,
        web_search_tool_type,
        experimental_supported_tools: model.experimental_supported_tools.clone(),
    };
    let service_tiers = model
        .service_tiers
        .into_iter()
        .map(|tier| tier.id)
        .collect();
    let lite = model
        .use_responses_lite
        .ok_or_else(|| "missing use_responses_lite".to_string())?;
    let truncation = model
        .truncation_policy
        .ok_or_else(|| "missing truncation_policy".to_string())?;
    let truncation_mode = match truncation.mode.as_str() {
        "bytes" => TruncationMode::Bytes,
        "tokens" => TruncationMode::Tokens,
        other => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                metadata,
                reason: format!(
                    "unknown required truncation mode {}",
                    bounded_wire_text(other, 128)
                ),
                source,
            });
        }
    };
    let descriptor = ModelDescriptor {
        slug: model.slug,
        display_name,
        instructions,
        context_window,
        auto_compact_token_limit,
        input_modalities: modalities,
        supports_reasoning: !reasoning.is_empty(),
        default_reasoning_effort: model.default_reasoning_level,
        supported_reasoning_efforts: reasoning,
        supports_verbosity,
        default_verbosity: model.default_verbosity,
        supports_parallel_tool_calls: parallel,
        tool_capabilities,
        service_tiers,
        reasoning_replay: if model.supports_encrypted_reasoning == Some(true)
            && model.supports_reasoning_replay == Some(true)
        {
            ReasoningReplaySupport::Enabled
        } else {
            ReasoningReplaySupport::Disabled
        },
        responses_dialect: if lite {
            ResponsesDialect::Lite
        } else {
            ResponsesDialect::Standard
        },
        tool_mode,
        multi_agent_version,
        truncation: TruncationPolicy {
            mode: truncation_mode,
            limit: truncation.limit,
        },
        comp_hash: model.comp_hash,
    };
    if let Err(error) = descriptor.validate() {
        return Ok(RemoteEntry::Incompatible {
            display_name: descriptor.display_name.clone(),
            metadata,
            reason: error.to_string(),
            source,
        });
    }
    Ok(RemoteEntry::Complete {
        descriptor: Box::new(descriptor),
        metadata,
        source,
    })
}

const fn default_true() -> bool {
    true
}

const fn default_effective_context_window_percent() -> u8 {
    95
}

pub(super) fn catalog_metadata_from_wire(model: &WireModel) -> CatalogMetadata {
    CatalogMetadata {
        description: model.description.clone(),
        visibility: model.visibility.clone(),
        supported_in_api: model.supported_in_api,
        priority: model.priority,
        reasoning_level_descriptions: model
            .supported_reasoning_levels
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|level| (level.effort.clone(), level.description.clone()))
            .collect(),
        additional_speed_tiers: model.additional_speed_tiers.clone(),
        service_tiers: model
            .service_tiers
            .iter()
            .map(|tier| CatalogServiceTier {
                id: tier.id.clone(),
                name: tier.name.clone(),
                description: tier.description.clone(),
            })
            .collect(),
        default_service_tier: model.default_service_tier.clone(),
        availability_nux: model.availability_nux.clone(),
        upgrade: model.upgrade.clone(),
        include_skills_usage_instructions: model.include_skills_usage_instructions,
        supports_reasoning_summary_parameter: model.supports_reasoning_summary_parameter,
        default_reasoning_summary: model.default_reasoning_summary.clone(),
        shell_type: model.shell_type.clone(),
        apply_patch_tool_type: model.apply_patch_tool_type.clone(),
        web_search_tool_type: model.web_search_tool_type.clone(),
        supports_image_detail_original: model.supports_image_detail_original,
        max_context_window: model.max_context_window,
        effective_context_window_percent: model.effective_context_window_percent,
        experimental_supported_tools: model.experimental_supported_tools.clone(),
        input_modalities: model.input_modalities.clone().unwrap_or_default(),
        supports_search_tool: model.supports_search_tool,
        auto_review_model_override: model.auto_review_model_override.clone(),
        unrecognized_fields: model.extra_fields.keys().cloned().collect(),
    }
}

pub(super) fn bounded_wire_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars
        .by_ref()
        .take(max_chars)
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}
