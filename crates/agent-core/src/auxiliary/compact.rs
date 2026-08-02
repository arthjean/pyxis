use std::future::Future;

use serde::Serialize;
use serde_json::Value;

use crate::provider::ResponseItem;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactRequest {
    pub model: String,
    pub input: Vec<ResponseItem>,
    pub instructions: String,
    pub tools: Option<Value>,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Value>,
    pub service_tier: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub text: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactResponse {
    pub output: Vec<ResponseItem>,
    pub turn_state: Option<String>,
}

/// Commits the complete validated candidate before replacing the live window.
/// The in-memory transcript remains untouched when the durable write fails.
pub async fn apply_after_durable_commit<E, F, Fut>(
    live: &mut Vec<ResponseItem>,
    response: CompactResponse,
    commit: F,
) -> Result<Option<String>, E>
where
    F: FnOnce(&CompactResponse) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    commit(&response).await?;
    *live = response.output;
    Ok(response.turn_state)
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemorySummarizeInput {
    pub model: String,
    pub raw_memories: Vec<RawMemory>,
    pub reasoning: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RawMemory {
    pub id: String,
    pub metadata: RawMemoryMetadata,
    pub items: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RawMemoryMetadata {
    pub source_path: String,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct MemorySummarizeOutput {
    #[serde(rename = "trace_summary", alias = "raw_memory")]
    pub raw_memory: String,
    pub memory_summary: String,
}
