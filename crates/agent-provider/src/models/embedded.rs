//! Versioned model descriptors bundled with the client.

use agent_core::model::{
    InputModality, ModelDescriptor, ModelToolCapabilities, ModelToolMode, MultiAgentVersion,
    ReasoningReplaySupport, ResponsesDialect, TruncationMode, TruncationPolicy,
};

const GENERIC_INSTRUCTIONS: &str = include_str!("../../../agent-cli/prompts/gpt5_generic.md");
const CODEX_INSTRUCTIONS: &str = include_str!("../../../agent-cli/prompts/codex_finetuned.md");

#[allow(clippy::too_many_arguments)]
fn descriptor(
    slug: &str,
    display_name: &str,
    instructions: &str,
    default_reasoning_effort: &str,
    supported_reasoning_efforts: &[&str],
    verbosity: &str,
    service_tiers: &[&str],
    dialect: ResponsesDialect,
    tool_mode: ModelToolMode,
    multi_agent_version: MultiAgentVersion,
    comp_hash: &str,
) -> ModelDescriptor {
    ModelDescriptor {
        slug: slug.into(),
        display_name: display_name.into(),
        instructions: instructions.into(),
        context_window: 272_000,
        auto_compact_token_limit: 244_800,
        input_modalities: vec![InputModality::Text, InputModality::Image],
        supports_reasoning: true,
        default_reasoning_effort: Some(default_reasoning_effort.into()),
        supported_reasoning_efforts: supported_reasoning_efforts
            .iter()
            .map(|effort| (*effort).into())
            .collect(),
        supports_verbosity: true,
        default_verbosity: Some(verbosity.into()),
        supports_parallel_tool_calls: true,
        tool_capabilities: ModelToolCapabilities::default(),
        service_tiers: service_tiers
            .iter()
            .map(|tier| (*tier).to_string())
            .collect(),
        reasoning_replay: ReasoningReplaySupport::Enabled,
        responses_dialect: dialect,
        tool_mode,
        multi_agent_version,
        truncation: TruncationPolicy {
            mode: TruncationMode::Tokens,
            limit: 10_000,
        },
        comp_hash: Some(comp_hash.into()),
    }
}

pub(super) fn embedded_descriptors() -> Vec<ModelDescriptor> {
    const STANDARD_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
    const FRONTIER_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
    const PRIORITY: &[&str] = &["priority"];
    const NO_SERVICE_TIERS: &[&str] = &[];
    vec![
        descriptor(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            GENERIC_INSTRUCTIONS,
            "low",
            FRONTIER_EFFORTS,
            "low",
            PRIORITY,
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V2,
            "3000",
        ),
        descriptor(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            GENERIC_INSTRUCTIONS,
            "medium",
            FRONTIER_EFFORTS,
            "low",
            PRIORITY,
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V2,
            "3000",
        ),
        descriptor(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            GENERIC_INSTRUCTIONS,
            "medium",
            &["low", "medium", "high", "xhigh", "max"],
            "low",
            PRIORITY,
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V1,
            "3000",
        ),
        descriptor(
            "gpt-5.5",
            "GPT-5.5",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "low",
            PRIORITY,
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            "2911",
        ),
        descriptor(
            "gpt-5.4",
            "GPT-5.4",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "low",
            PRIORITY,
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            "2911",
        ),
        descriptor(
            "gpt-5.4-mini",
            "GPT-5.4 Mini",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "medium",
            NO_SERVICE_TIERS,
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            "2911",
        ),
        descriptor(
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            CODEX_INSTRUCTIONS,
            "high",
            STANDARD_EFFORTS,
            "low",
            NO_SERVICE_TIERS,
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            "2026-07-24",
        ),
    ]
}
