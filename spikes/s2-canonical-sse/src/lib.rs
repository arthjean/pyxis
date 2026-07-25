//! US-002: canonical layer, a provider SSE stream -> typed `StreamEvent`.
//!
//! Proves that the in-house layer (`reqwest` + `eventsource-stream`, without an SDK) holds up.
//! The `StreamEvent` vocabulary is the one frozen in `docs/PROVIDERS.md 2`
//! (Anthropic-like). The adapter here targets the **OpenAI Chat Completions** wire
//! format (the same one Ollama serves in `/v1/chat/completions` mode), reused by S1 and S3.
//!
//! Key invariant: at `ToolCallEnd`, the concatenation of the `ToolCallDelta.args_json`
//! of a same id forms a complete and valid JSON (see `PROVIDERS.md 2`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use futures_util::stream::BoxStream;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

// ───────────────────────────── Canonical vocabulary ─────────────────────────

/// The only streaming vocabulary the core knows (see `PROVIDERS.md 2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, args_json: String },
    ToolCallEnd { id: String },
    Usage { usage: TokenUsage },
    Done { stop: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
}

impl StopReason {
    fn from_finish(s: &str) -> Self {
        match s {
            "stop" => StopReason::EndTurn,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::Refusal,
            _ => StopReason::EndTurn,
        }
    }
}

/// Classified adapter error, never a panic (US-002 AC2/AC3).
#[derive(Debug, Clone)]
pub enum AdapterError {
    /// Transport/connection failure (Ollama down, DNS, refusal).
    Transport(String),
    /// Non-2xx HTTP response (carries the code for later classification).
    Http { status: u16, body: String },
    /// Malformed JSON chunk: ignored or surfaced without crashing the parser.
    Json(String),
    /// Stream cut in the middle of a message.
    Stream(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::Transport(m) => write!(f, "transport: {m}"),
            AdapterError::Http { status, body } => write!(f, "http {status}: {body}"),
            AdapterError::Json(m) => write!(f, "chunk JSON malformé: {m}"),
            AdapterError::Stream(m) => write!(f, "flux interrompu: {m}"),
        }
    }
}
impl std::error::Error for AdapterError {}

// ───────────────────────── OpenAI Chat adapter (stateful) ────────────────────

/// Deserialization of the OpenAI Chat Completions wire format (stream chunk).
#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    // Reasoning variants depending on the provider (the OpenAI o-series exposes nothing;
    // some models through Ollama expose `reasoning_content`).
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize)]
struct RawToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<RawFn>,
}

#[derive(Deserialize)]
struct RawFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

/// Stateful adapter: reassembles the tool calls fragmented by `index` and guarantees
/// the complete `args_json` invariant at `ToolCallEnd`.
#[derive(Default)]
pub struct OpenAiChatAdapter {
    index_to_id: HashMap<u32, String>,
    started: Vec<String>,
    ended: HashSet<String>,
}

impl OpenAiChatAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    fn id_for(&mut self, tc: &RawToolCall) -> String {
        if let Some(id) = &tc.id {
            self.index_to_id.insert(tc.index, id.clone());
            id.clone()
        } else {
            self.index_to_id
                .get(&tc.index)
                .cloned()
                .unwrap_or_else(|| format!("call_{}", tc.index))
        }
    }

    /// Translates an SSE `data:` (one JSON chunk) into 0..n canonical `StreamEvent`.
    /// `[DONE]` produces nothing: `Done` is emitted on `finish_reason`.
    pub fn ingest(&mut self, data: &str) -> Result<Vec<StreamEvent>, AdapterError> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let chunk: Chunk =
            serde_json::from_str(data).map_err(|e| AdapterError::Json(e.to_string()))?;

        let mut out = Vec::new();
        for choice in &chunk.choices {
            if let Some(t) = &choice.delta.content
                && !t.is_empty()
            {
                out.push(StreamEvent::TextDelta { text: t.clone() });
            }
            if let Some(r) = choice
                .delta
                .reasoning
                .as_ref()
                .or(choice.delta.reasoning_content.as_ref())
                && !r.is_empty()
            {
                out.push(StreamEvent::ReasoningDelta { text: r.clone() });
            }
            for tc in &choice.delta.tool_calls {
                let id = self.id_for(tc);
                if let Some(f) = &tc.function {
                    if let Some(name) = &f.name
                        && !self.started.contains(&id)
                    {
                        self.started.push(id.clone());
                        out.push(StreamEvent::ToolCallStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                    }
                    if let Some(args) = &f.arguments
                        && !args.is_empty()
                    {
                        out.push(StreamEvent::ToolCallDelta {
                            id: id.clone(),
                            args_json: args.clone(),
                        });
                    }
                }
            }
            if let Some(reason) = &choice.finish_reason {
                if reason == "tool_calls" {
                    let to_close: Vec<String> = self
                        .started
                        .iter()
                        .filter(|id| !self.ended.contains(*id))
                        .cloned()
                        .collect();
                    for id in to_close {
                        self.ended.insert(id.clone());
                        out.push(StreamEvent::ToolCallEnd { id });
                    }
                }
                out.push(StreamEvent::Done {
                    stop: StopReason::from_finish(reason),
                });
            }
        }

        if let Some(u) = &chunk.usage {
            out.push(StreamEvent::Usage {
                usage: TokenUsage {
                    input: u.prompt_tokens,
                    output: u.completion_tokens,
                    total: u.total_tokens,
                },
            });
        }
        Ok(out)
    }
}

// ───────────────────────────── Live streaming (reqwest) ──────────────────────

/// Builds the OpenAI-compatible request body (stream + usage).
pub fn build_body(
    model: &str,
    messages: serde_json::Value,
    tools: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(tools) = tools {
        body["tools"] = tools;
    }
    body
}

/// Opens a `StreamEvent` stream from an OpenAI-compatible `/chat/completions` endpoint.
/// `base_url` = e.g. `http://localhost:11434/v1` (Ollama) or `https://api.openai.com/v1`.
pub async fn stream_chat(
    base_url: &str,
    api_key: Option<&str>,
    body: serde_json::Value,
) -> Result<BoxStream<'static, Result<StreamEvent, AdapterError>>, AdapterError> {
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;

    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{base_url}/chat/completions"))
        .json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| AdapterError::Transport(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let mut adapter = OpenAiChatAdapter::new();
    let mut es = resp.bytes_stream().eventsource();

    let s = async_stream::stream! {
        while let Some(ev) = es.next().await {
            match ev {
                Ok(event) => match adapter.ingest(&event.data) {
                    Ok(events) => {
                        for e in events {
                            yield Ok(e);
                        }
                    }
                    // Malformed chunk: typed error, the parser does not crash (AC3).
                    Err(e) => yield Err(e),
                },
                // Stream cut in the middle of a message: classified, no panic (AC2).
                Err(e) => {
                    yield Err(AdapterError::Stream(e.to_string()));
                    return;
                }
            }
        }
    };

    Ok(s.boxed())
}

// ─────────────────────────────────── Tests ──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_deltas_map_to_textdelta() {
        let mut a = OpenAiChatAdapter::new();
        let ev = a
            .ingest(r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(
            ev,
            vec![StreamEvent::TextDelta {
                text: "Hello".into()
            }]
        );
    }

    #[test]
    fn finish_stop_emits_done_endturn() {
        let mut a = OpenAiChatAdapter::new();
        let ev = a
            .ingest(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
            .unwrap();
        assert_eq!(
            ev,
            vec![StreamEvent::Done {
                stop: StopReason::EndTurn
            }]
        );
    }

    #[test]
    fn usage_chunk_maps_to_usage() {
        let mut a = OpenAiChatAdapter::new();
        let ev = a
            .ingest(r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}}"#)
            .unwrap();
        assert_eq!(
            ev,
            vec![StreamEvent::Usage {
                usage: TokenUsage {
                    input: 12,
                    output: 8,
                    total: 20
                }
            }]
        );
    }

    #[test]
    fn tool_call_fragmented_reassembles_to_valid_json_at_end() {
        let mut a = OpenAiChatAdapter::new();
        // 1st fragment: id + name + start of args
        let _ = a
            .ingest(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"cmd\":\""}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        // 2nd fragment: rest of the args, WITHOUT an id (resolved by index)
        let _ = a
            .ingest(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"echo hi\"}"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        // closing
        let end = a
            .ingest(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
            .unwrap();

        // We replay everything to rebuild the full stream and check the invariant.
        let mut a2 = OpenAiChatAdapter::new();
        let mut all = Vec::new();
        for c in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"cmd\":\""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"echo hi\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            all.extend(a2.ingest(c).unwrap());
        }

        // Start present, End present (from the last chunk), Done = ToolUse.
        assert!(all.contains(&StreamEvent::ToolCallStart {
            id: "call_1".into(),
            name: "bash".into()
        }));
        assert!(end.contains(&StreamEvent::ToolCallEnd {
            id: "call_1".into()
        }));
        assert!(all.contains(&StreamEvent::Done {
            stop: StopReason::ToolUse
        }));

        // Invariant: concatenated args_json = valid JSON.
        let args: String = all
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, args_json } if id == "call_1" => {
                    Some(args_json.clone())
                }
                _ => None,
            })
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&args).expect("args_json valide");
        assert_eq!(parsed["cmd"], "echo hi");
    }

    #[test]
    fn malformed_chunk_yields_typed_error_not_panic() {
        let mut a = OpenAiChatAdapter::new();
        let err = a.ingest("{ this is not json ]").unwrap_err();
        assert!(
            matches!(err, AdapterError::Json(_)),
            "attendu Json, eu {err:?}"
        );
    }

    #[test]
    fn done_sentinel_is_noop() {
        let mut a = OpenAiChatAdapter::new();
        assert_eq!(a.ingest("[DONE]").unwrap(), Vec::new());
        assert_eq!(a.ingest("").unwrap(), Vec::new());
    }
}
