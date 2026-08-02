use agent_core::auxiliary::compact::{
    CompactRequest, CompactResponse, MemorySummarizeInput, MemorySummarizeOutput,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};
use agent_core::provider::ResponseItem;
use reqwest::header::HeaderValue;
use serde::Serialize;
use serde_json::Value;

use super::ConfiguredOpenAiProvider;
use super::json::decode;
use super::validation::{nonempty, text};

#[derive(Serialize)]
struct CompactWireRequest<'a> {
    model: &'a str,
    input: Vec<&'a Value>,
    #[serde(skip_serializing_if = "str::is_empty")]
    instructions: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a Value>,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a Value>,
}

#[derive(Serialize)]
struct MemoryWireRequest<'a> {
    model: &'a str,
    #[serde(rename = "traces")]
    raw_memories: &'a [agent_core::auxiliary::compact::RawMemory],
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a Value>,
}

pub(super) async fn remote(
    provider: &ConfiguredOpenAiProvider,
    request: &CompactRequest,
) -> Result<CompactResponse, AuxiliaryError> {
    let operation = AuxiliaryOperation::RemoteCompact;
    super::ensure_supported(provider, operation)?;
    nonempty(operation, "model", &request.model, 256)?;
    if !request.instructions.is_empty() {
        text(operation, "instructions", &request.instructions, 1_000_000)?;
    }
    if request.input.is_empty() {
        return Err(AuxiliaryError::invalid(
            operation,
            "input",
            "at least one response item is required",
        ));
    }
    let wire = CompactWireRequest {
        model: &request.model,
        input: request
            .input
            .iter()
            .map(|item| item.payload().payload())
            .collect(),
        instructions: &request.instructions,
        tools: request.tools.as_ref(),
        parallel_tool_calls: request.parallel_tool_calls,
        reasoning: request.reasoning.as_ref(),
        service_tier: request.service_tier.as_deref(),
        prompt_cache_key: request.prompt_cache_key.as_deref(),
        text: request.text.as_ref(),
    };
    let response = provider
        .auxiliary_json(operation, "responses/compact", &wire)
        .await?;
    let value: Value = decode(operation, &response.body)?;
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .filter(|output| !output.is_empty())
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing or empty output array"))?;
    let output = output
        .iter()
        .map(|item| {
            ResponseItem::from_wire(item)
                .map_err(|_| AuxiliaryError::decode(operation, "invalid response item"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let turn_state = bounded_turn_state(response.headers.get("x-codex-turn-state"))?;
    Ok(CompactResponse { output, turn_state })
}

pub(super) async fn summarize_memories(
    provider: &ConfiguredOpenAiProvider,
    request: &MemorySummarizeInput,
) -> Result<Vec<MemorySummarizeOutput>, AuxiliaryError> {
    let operation = AuxiliaryOperation::Memories;
    super::ensure_supported(provider, operation)?;
    nonempty(operation, "model", &request.model, 256)?;
    if request.raw_memories.is_empty() {
        return Err(AuxiliaryError::invalid(
            operation,
            "traces",
            "at least one trace is required",
        ));
    }
    for memory in &request.raw_memories {
        nonempty(operation, "traces.id", &memory.id, 256)?;
        nonempty(
            operation,
            "traces.metadata.source_path",
            &memory.metadata.source_path,
            4096,
        )?;
        if memory.items.is_empty() {
            return Err(AuxiliaryError::invalid(
                operation,
                "traces.items",
                "trace items cannot be empty",
            ));
        }
    }
    let wire = MemoryWireRequest {
        model: &request.model,
        raw_memories: &request.raw_memories,
        reasoning: request.reasoning.as_ref(),
    };
    let response = provider
        .auxiliary_json(operation, "memories/trace_summarize", &wire)
        .await?;
    let value: Value = decode(operation, &response.body)?;
    let output = value
        .get("output")
        .cloned()
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing output array"))?;
    let output: Vec<MemorySummarizeOutput> = serde_json::from_value(output)
        .map_err(|_| AuxiliaryError::decode(operation, "invalid memory summary output"))?;
    if output.len() != request.raw_memories.len() {
        return Err(AuxiliaryError::decode(
            operation,
            "output count does not match trace count",
        ));
    }
    Ok(output)
}

fn bounded_turn_state(value: Option<&HeaderValue>) -> Result<Option<String>, AuxiliaryError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        AuxiliaryError::decode(
            AuxiliaryOperation::RemoteCompact,
            "invalid turn state header",
        )
    })?;
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(AuxiliaryError::decode(
            AuxiliaryOperation::RemoteCompact,
            "invalid turn state header",
        ));
    }
    Ok(Some(value.to_string()))
}
