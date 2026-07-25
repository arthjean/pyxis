//! Mapping of the SSE events of the Responses API (ChatGPT/Codex backend) into the
//! canonical `StreamEvent` vocabulary (PROVIDERS 2). Stateful: tracks the
//! active function call and accumulates its arguments to guarantee the invariant
//! "`args_json` complete & valid at `ToolCallEnd`".
//!
//! Event types transcribed verbatim from Pi (`openai-responses-shared.ts` +
//! `openai-codex-responses.ts`). The irrelevant events (created, part.added,
//! content_part.added, ...) are silently ignored, like Pi.

use agent_core::provider::{ProviderError, StopReason, StreamEvent, TokenUsage};
use serde_json::Value;
use std::collections::HashMap;

struct ActiveCall {
    call_id: String,
    args: String,
}

/// Stateful mapper for one response stream. Reinstantiated on every turn.
#[derive(Default)]
pub struct CodexEventMapper {
    active: HashMap<String, ActiveCall>,
    output_index_to_item: HashMap<u64, String>,
    last_active_item: Option<String>,
    /// Has at least one tool call been emitted? (overrides stop `completed` -> `ToolUse`).
    saw_tool_call: bool,
    /// US-031: capture the encrypted reasoning items for replay? The raw mapper
    /// keeps an OFF default; the ChatGPT provider enables it explicitly.
    replay: bool,
}

impl CodexEventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a mapper with the reasoning replay (US-031) enabled or not.
    pub fn with_replay(replay: bool) -> Self {
        Self {
            replay,
            ..Self::default()
        }
    }

    /// Translates an SSE `data:` payload (one JSON Responses event) into 0..n
    /// `StreamEvent`. A terminal event (`response.completed`/`.done`/
    /// `.incomplete`) emits `Usage?` then `Done`. An error (`error`/
    /// `response.failed`) surfaces a typed `ProviderError`, never a panic.
    pub fn ingest(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");

        match typ {
            "response.output_text.delta" => {
                Ok(delta_event(&v, |text| StreamEvent::TextDelta { text }))
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                Ok(delta_event(&v, |text| StreamEvent::ReasoningDelta { text }))
            }
            "response.reasoning_summary_part.done" => Ok(vec![StreamEvent::ReasoningDelta {
                text: "\n\n".to_string(),
            }]),
            "response.output_item.added" => Ok(self.on_item_added(&v)),
            "response.function_call_arguments.delta" => {
                if let (Some(key), Some(delta)) = (
                    self.event_item_key(&v, "function_call_arguments.delta")?,
                    v.get("delta").and_then(Value::as_str),
                ) && let Some(active) = self.active.get_mut(&key)
                {
                    active.args.push_str(delta);
                }
                Ok(Vec::new())
            }
            "response.function_call_arguments.done" => {
                // Authoritative source of the complete arguments (replaces the accumulated ones).
                if let (Some(key), Some(args)) = (
                    self.event_item_key(&v, "function_call_arguments.done")?,
                    v.get("arguments").and_then(Value::as_str),
                ) && let Some(active) = self.active.get_mut(&key)
                {
                    active.args = args.to_string();
                }
                Ok(Vec::new())
            }
            "response.output_item.done" => self.on_item_done(&v),
            "response.completed" | "response.done" | "response.incomplete" => {
                Ok(self.on_terminal(&v))
            }
            "error" => Err(stream_error(&v)),
            "response.failed" => Err(failed_error(&v)),
            // created, content_part.added, reasoning_summary_part.added, ... -> ignored.
            _ => Ok(Vec::new()),
        }
    }

    fn on_item_added(&mut self, v: &Value) -> Vec<StreamEvent> {
        let item = match v.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Vec::new();
        }
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // arguments often "" at opening time; we accumulate what follows.
        let args = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let item_id = item_id(item).unwrap_or(call_id.as_str()).to_string();
        if let Some(index) = v.get("output_index").and_then(Value::as_u64) {
            self.output_index_to_item.insert(index, item_id.clone());
        }
        self.saw_tool_call = true;
        self.last_active_item = Some(item_id.clone());
        self.active.insert(
            item_id,
            ActiveCall {
                call_id: call_id.clone(),
                args,
            },
        );
        vec![StreamEvent::ToolCallStart { id: call_id, name }]
    }

    fn on_item_done(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let item_type = v
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str);
        // US-031: encrypted reasoning item, captured only when replay is active.
        // `encrypted_content`/`id` opaque.
        if item_type == Some("reasoning") {
            if !self.replay {
                return Ok(Vec::new());
            }
            let item = match v.get("item") {
                Some(i) => i,
                None => return Ok(Vec::new()),
            };
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let enc = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // a reasoning without encrypted content is not reinjectable -> ignored.
            if id.is_empty() || enc.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![StreamEvent::EncryptedReasoning {
                id: id.to_string(),
                encrypted_content: enc.to_string(),
            }]);
        }
        if item_type != Some("function_call") {
            return Ok(Vec::new());
        }
        // final arguments: item.done takes priority, otherwise the accumulated ones.
        let item_args = v
            .get("item")
            .and_then(|i| i.get("arguments"))
            .and_then(Value::as_str);
        let Some(key) = self.event_item_key(v, "output_item.done")? else {
            return self.reconstruct_done_function_call(v);
        };
        let Some(active) = self.active.remove(&key) else {
            return self.reconstruct_done_function_call(v);
        };
        if self.last_active_item.as_deref() == Some(key.as_str()) {
            self.last_active_item = None;
        }
        let args = match item_args {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => active.args,
        };
        let mut out = Vec::new();
        // A single ToolCallDelta carrying everything -> JSON invariant guaranteed.
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                id: active.call_id.clone(),
                args_json: args,
            });
        }
        out.push(StreamEvent::ToolCallEnd { id: active.call_id });
        Ok(out)
    }

    fn reconstruct_done_function_call(
        &mut self,
        v: &Value,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let Some(item) = v.get("item") else {
            return Err(ProviderError::Decode(
                "function_call done without active call or item".to_string(),
            ));
        };
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        if call_id.trim().is_empty() || name.trim().is_empty() {
            return Err(ProviderError::Decode(
                "function_call done without active call id or name".to_string(),
            ));
        }
        let args = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.saw_tool_call = true;
        let mut out = vec![StreamEvent::ToolCallStart {
            id: call_id.to_string(),
            name: name.to_string(),
        }];
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                id: call_id.to_string(),
                args_json: args.to_string(),
            });
        }
        out.push(StreamEvent::ToolCallEnd {
            id: call_id.to_string(),
        });
        Ok(out)
    }

    fn event_item_key(&self, v: &Value, event_name: &str) -> Result<Option<String>, ProviderError> {
        if let Some(id) = v.get("item_id").and_then(Value::as_str)
            && self.active.contains_key(id)
        {
            return Ok(Some(id.to_string()));
        }
        if let Some(id) = v.get("item").and_then(item_id)
            && self.active.contains_key(id)
        {
            return Ok(Some(id.to_string()));
        }
        if let Some(index) = v.get("output_index").and_then(Value::as_u64)
            && let Some(id) = self.output_index_to_item.get(&index)
        {
            return Ok(Some(id.clone()));
        }
        let call_id = v
            .get("call_id")
            .or_else(|| v.get("item").and_then(|i| i.get("call_id")))
            .and_then(Value::as_str);
        if let Some(call_id) = call_id {
            let mut matches = self
                .active
                .iter()
                .filter(|(_, active)| active.call_id == call_id);
            if let Some((key, _)) = matches.next()
                && matches.next().is_none()
            {
                return Ok(Some(key.clone()));
            }
        }
        if self.active.len() == 1 {
            return Ok(self.active.keys().next().cloned());
        }
        if self.active.is_empty() {
            return Ok(None);
        }
        Err(ProviderError::Decode(format!(
            "ambiguous {event_name} without item id"
        )))
    }

    fn on_terminal(&mut self, v: &Value) -> Vec<StreamEvent> {
        let response = v.get("response");
        let status = response
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed");

        let mut out = Vec::new();
        if let Some(usage) = response.and_then(|r| r.get("usage")).and_then(parse_usage) {
            out.push(StreamEvent::Usage { usage });
        }
        out.push(StreamEvent::Done {
            stop: self.stop_for(status),
        });
        out
    }

    fn stop_for(&self, status: &str) -> StopReason {
        match status {
            "incomplete" => StopReason::MaxTokens,
            "failed" | "cancelled" => StopReason::Refusal,
            // completed / in_progress / queued / absent -> normal end;
            // overridden to ToolUse when tool calls were emitted.
            _ if self.saw_tool_call => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        }
    }
}

fn delta_event(v: &Value, ctor: impl Fn(String) -> StreamEvent) -> Vec<StreamEvent> {
    match v.get("delta").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => vec![ctor(d.to_string())],
        _ => Vec::new(),
    }
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

/// `response.usage` -> `TokenUsage`. `input_tokens` includes the cached ones (we keep the
/// full context size for the compaction threshold, ARCHITECTURE 3.3).
fn parse_usage(usage: &Value) -> Option<TokenUsage> {
    let input = usage.get("input_tokens").and_then(Value::as_u64)? as u32;
    let output = usage.get("output_tokens").and_then(Value::as_u64)? as u32;
    Some(TokenUsage { input, output })
}

fn stream_error(v: &Value) -> ProviderError {
    let code = v.get("code").and_then(Value::as_str).unwrap_or("");
    let message = v.get("message").and_then(Value::as_str).unwrap_or("");
    classify_message(code, message)
}

fn failed_error(v: &Value) -> ProviderError {
    let err = v.get("response").and_then(|r| r.get("error"));
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("response.failed");
    classify_failed_message(code, message)
}

/// Distinguishes a context overflow (-> withholding/reactive compaction) from a
/// generic stream error.
fn classify_message(code: &str, message: &str) -> ProviderError {
    let hay = format!("{code} {message}").to_lowercase();
    if hay.contains("context") && (hay.contains("length") || hay.contains("long")) {
        ProviderError::ContextLengthExceeded
    } else {
        ProviderError::Stream(format!("{code}: {message}"))
    }
}

fn classify_failed_message(code: &str, message: &str) -> ProviderError {
    let hay = format!("{code} {message}").to_lowercase();
    let detail = format!("{code}: {message}");
    if hay.contains("context") && (hay.contains("length") || hay.contains("long")) {
        ProviderError::ContextLengthExceeded
    } else if hay.contains("rate_limit")
        || hay.contains("rate limit")
        || hay.contains("too many requests")
        || hay.contains("quota")
    {
        ProviderError::Http {
            status: 429,
            message: detail,
            retry_after_ms: None,
        }
    } else if hay.contains("auth")
        || hay.contains("unauthorized")
        || hay.contains("invalid token")
        || hay.contains("expired")
    {
        ProviderError::Http {
            status: 401,
            message: detail,
            retry_after_ms: None,
        }
    } else if hay.contains("permission") || hay.contains("forbidden") {
        ProviderError::Http {
            status: 403,
            message: detail,
            retry_after_ms: None,
        }
    } else if hay.contains("overload") {
        ProviderError::Http {
            status: 529,
            message: detail,
            retry_after_ms: None,
        }
    } else if hay.contains("server")
        || hay.contains("internal")
        || hay.contains("temporarily")
        || hay.contains("unavailable")
        || hay.contains("timeout")
    {
        ProviderError::Http {
            status: 503,
            message: detail,
            retry_after_ms: None,
        }
    } else {
        ProviderError::Http {
            status: 400,
            message: detail,
            retry_after_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest_all(events: &[&str]) -> Vec<StreamEvent> {
        let mut m = CodexEventMapper::new();
        let mut out = Vec::new();
        for e in events {
            out.extend(m.ingest(e).unwrap());
        }
        out
    }

    #[test]
    fn text_delta_maps() {
        let ev = ingest_all(&[r#"{"type":"response.output_text.delta","delta":"Hello"}"#]);
        assert_eq!(
            ev,
            vec![StreamEvent::TextDelta {
                text: "Hello".into()
            }]
        );
    }

    #[test]
    fn reasoning_deltas_map() {
        let ev = ingest_all(&[
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.reasoning_text.delta","delta":" encore"}"#,
        ]);
        assert_eq!(
            ev,
            vec![
                StreamEvent::ReasoningDelta {
                    text: "thinking".into()
                },
                StreamEvent::ReasoningDelta {
                    text: " encore".into()
                },
            ]
        );
    }

    #[test]
    fn completed_without_tools_is_endturn_with_usage() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_text.delta","delta":"ok"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":120,"output_tokens":8}}}"#,
        ]);
        assert!(ev.contains(&StreamEvent::Usage {
            usage: TokenUsage {
                input: 120,
                output: 8
            }
        }));
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::EndTurn
            })
        );
    }

    #[test]
    fn function_call_full_lifecycle_reassembles_valid_json() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"cmd\":\""}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"ls\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"cmd\":\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":50,"output_tokens":12}}}"#,
        ]);

        assert!(ev.contains(&StreamEvent::ToolCallStart {
            id: "call_7".into(),
            name: "bash".into()
        }));
        assert!(ev.contains(&StreamEvent::ToolCallEnd {
            id: "call_7".into()
        }));
        // stop = ToolUse because a tool call was emitted.
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::ToolUse
            })
        );

        // invariant: concatenated args_json = valid JSON.
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, args_json } if id == "call_7" => {
                    Some(args_json.clone())
                }
                _ => None,
            })
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&args).expect("JSON valide");
        assert_eq!(parsed["cmd"], "ls");
    }

    #[test]
    fn interleaved_function_calls_are_tracked_by_item_id() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"path\":\""}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"path\":\""}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"a.txt\"}"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"b.txt\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":"{\"path\":\"b.txt\"}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);

        let args_for = |call_id: &str| -> String {
            ev.iter()
                .filter_map(|e| match e {
                    StreamEvent::ToolCallDelta { id, args_json } if id == call_id => {
                        Some(args_json.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(args_for("call_a"), "{\"path\":\"a.txt\"}");
        assert_eq!(args_for("call_b"), "{\"path\":\"b.txt\"}");
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn ambiguous_parallel_tool_delta_fails_closed() {
        let mut m = CodexEventMapper::new();
        assert!(
            m.ingest(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#
            )
            .unwrap()
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { id, .. } if id == "call_a"))
        );
        assert!(
            m.ingest(
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#
            )
            .unwrap()
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { id, .. } if id == "call_b"))
        );
        let err = m
            .ingest(r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#)
            .unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[test]
    fn parallel_tool_delta_can_fallback_to_unique_call_id() {
        let mut m = CodexEventMapper::new();
        m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
        )
        .unwrap();
        m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#,
        )
        .unwrap();
        assert!(
            m.ingest(
                r#"{"type":"response.function_call_arguments.done","call_id":"call_b","arguments":"{\"path\":\"b.txt\"}"}"#
            )
            .unwrap()
            .is_empty()
        );
        let ev = m
            .ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"write"}}"#,
            )
            .unwrap();
        assert!(ev.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallDelta { id, args_json }
                if id == "call_b" && args_json == "{\"path\":\"b.txt\"}"
            )
        }));
    }

    #[test]
    fn args_only_in_item_done_still_emitted() {
        // backend that sends no deltas: args only in output_item.done.
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","id":"fc","name":"x","arguments":""}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","id":"fc","name":"x","arguments":"{\"a\":1}"}}"#,
        ]);
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { args_json, .. } => Some(args_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"a\":1}");
    }

    #[test]
    fn function_call_done_without_added_reconstructs_call() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert!(ev.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallStart { id, name } if id == "c1" && name == "read"
            )
        }));
        assert!(ev.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallDelta { id, args_json }
                if id == "c1" && args_json == "{\"path\":\"Cargo.toml\"}"
            )
        }));
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn function_call_done_without_added_or_name_fails_closed() {
        let mut m = CodexEventMapper::new();
        let err = m
            .ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","arguments":"{}"}}"#,
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[test]
    fn incomplete_status_maps_to_maxtokens() {
        let ev =
            ingest_all(&[r#"{"type":"response.incomplete","response":{"status":"incomplete"}}"#]);
        assert_eq!(
            ev,
            vec![StreamEvent::Done {
                stop: StopReason::MaxTokens
            }]
        );
    }

    #[test]
    fn error_event_yields_typed_error_not_panic() {
        let mut m = CodexEventMapper::new();
        let err = m
            .ingest(r#"{"type":"error","code":"server_error","message":"boom"}"#)
            .unwrap_err();
        assert!(matches!(err, ProviderError::Stream(_)));
    }

    #[test]
    fn context_length_error_is_classified_for_withholding() {
        let mut m = CodexEventMapper::new();
        let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"maximum context length"}}}"#,
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::ContextLengthExceeded));
        assert!(err.is_context_error());
    }

    #[test]
    fn response_failed_invalid_request_is_not_stream_retryable() {
        let mut m = CodexEventMapper::new();
        let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"invalid_request_error","message":"bad schema"}}}"#,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Http {
                status: 400,
                retry_after_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn response_failed_rate_limit_keeps_rate_limited_status() {
        let mut m = CodexEventMapper::new();
        let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"too many requests"}}}"#,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Http {
                status: 429,
                retry_after_ms: None,
                ..
            }
        ));
    }

    // US-031: encrypted reasoning item captured only when replay is active.
    #[test]
    fn reasoning_item_captured_only_when_replay_on() {
        let done = r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC"}}"#;
        // OFF (default) -> ignored (flat path).
        assert!(CodexEventMapper::new().ingest(done).unwrap().is_empty());
        // ON -> EncryptedReasoning emitted.
        let ev = CodexEventMapper::with_replay(true).ingest(done).unwrap();
        assert_eq!(
            ev,
            vec![StreamEvent::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into()
            }]
        );
        // reasoning without encrypted content -> ignored even when ON (not reinjectable).
        let empty =
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_2"}}"#;
        assert!(
            CodexEventMapper::with_replay(true)
                .ingest(empty)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_chunk_is_typed_error() {
        let mut m = CodexEventMapper::new();
        assert!(matches!(
            m.ingest("{not json").unwrap_err(),
            ProviderError::Decode(_)
        ));
        // empty line -> no-op.
        assert!(m.ingest("").unwrap().is_empty());
    }
}
