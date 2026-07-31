//! Mapping of the SSE events of the Responses API (ChatGPT/Codex backend) into the
//! canonical `StreamEvent` vocabulary (PROVIDERS 2). Stateful: tracks the
//! active function call and accumulates its arguments to guarantee the invariant
//! "`input_delta` complete & valid at `ToolCallEnd`".
//!
//! Event types transcribed verbatim from Pi (`openai-responses-shared.ts` +
//! `openai-codex-responses.ts`). The irrelevant events (created, part.added,
//! content_part.added, ...) are silently ignored, like Pi.

use agent_core::message::ToolCallFormat;
use agent_core::provider::{ProviderError, StopReason, StreamEvent, TokenUsage};
use serde_json::Value;
use std::collections::HashMap;

struct ActiveCall {
    call_id: String,
    args: String,
    format: ToolCallFormat,
}

/// Responses output item types this mapper knowingly handles: projected here
/// (`function_call`, `custom_tool_call`, `reasoning`) or carried by their own
/// delta events (`message`). The conformance suite reads it so an item the
/// wire starts sending, and that we silently ignore, fails a test instead of
/// disappearing from the stream.
pub const MAPPED_OUTPUT_ITEM_TYPES: &[&str] =
    &["message", "reasoning", "function_call", "custom_tool_call"];

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
                    if active.format != ToolCallFormat::Json {
                        return Err(ProviderError::Decode(format!(
                            "function arguments delta on custom tool call {}",
                            active.call_id
                        )));
                    }
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
            // Freeform input arrives as text fragments on its own event name.
            "response.custom_tool_call_input.delta" => {
                if let (Some(key), Some(delta)) = (
                    self.event_item_key(&v, "custom_tool_call_input.delta")?,
                    v.get("delta").and_then(Value::as_str),
                ) && let Some(active) = self.active.get_mut(&key)
                {
                    if active.format != ToolCallFormat::Text {
                        return Err(ProviderError::Decode(format!(
                            "custom tool input delta on function call {}",
                            active.call_id
                        )));
                    }
                    active.args.push_str(delta);
                }
                Ok(Vec::new())
            }
            "response.custom_tool_call_input.done" => {
                if let (Some(key), Some(input)) = (
                    self.event_item_key(&v, "custom_tool_call_input.done")?,
                    v.get("input").and_then(Value::as_str),
                ) && let Some(active) = self.active.get_mut(&key)
                {
                    active.args = input.to_string();
                }
                Ok(Vec::new())
            }
            "response.output_item.done" => self.on_item_done(&v),
            "response.completed" | "response.done" | "response.incomplete" => {
                self.on_terminal(&v, typ)
            }
            // Quota state pushed mid-stream. Richer than the response headers,
            // which never name the plan, so it is read here as well.
            "codex.rate_limits" => Ok(crate::quota::parse_quota_event(&v)
                .map(|snapshot| vec![StreamEvent::Quota { snapshot }])
                .unwrap_or_default()),
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
        let Some(format) = call_format(item) else {
            return Vec::new();
        };
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
        // Input is often "" at opening time; we accumulate what follows.
        let args = initial_call_input(item, format).to_string();
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
                format,
            },
        );
        vec![StreamEvent::ToolCallStart {
            id: call_id,
            name,
            format,
        }]
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
        let Some(item) = v.get("item") else {
            return Ok(Vec::new());
        };
        let Some(format) = call_format(item) else {
            // Everything outside the mapped set is content we cannot read. It is
            // REPORTED rather than dropped: a web search or an image generation
            // served by the backend would otherwise leave no trace at all.
            return Ok(unmapped_item_event(item));
        };
        // The terminal item is authoritative over every delta that preceded it.
        let item_args = terminal_call_input(item, format);
        let Some(key) = self.event_item_key(v, "output_item.done")? else {
            return self.reconstruct_done_call(item, format);
        };
        let Some(active) = self.active.remove(&key) else {
            return self.reconstruct_done_call(item, format);
        };
        if active.format != format {
            return Err(ProviderError::Decode(format!(
                "tool call {} changed format mid-stream",
                active.call_id
            )));
        }
        if self.last_active_item.as_deref() == Some(key.as_str()) {
            self.last_active_item = None;
        }
        let args = match item_args {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => active.args,
        };
        let mut out = Vec::new();
        // A single ToolCallDelta carrying everything: valid JSON for a
        // function, the exact text for a freeform call, and no duplicate.
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                id: active.call_id.clone(),
                input_delta: args,
            });
        }
        out.push(StreamEvent::ToolCallEnd { id: active.call_id });
        Ok(out)
    }

    /// Terminal item for a call whose opening was never observed. The item
    /// alone must carry an id and a name, otherwise nothing is dispatchable.
    fn reconstruct_done_call(
        &mut self,
        item: &Value,
        format: ToolCallFormat,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        if call_id.trim().is_empty() || name.trim().is_empty() {
            return Err(ProviderError::Decode(
                "tool call done without active call id or name".to_string(),
            ));
        }
        let args = terminal_call_input(item, format).unwrap_or_default();
        self.saw_tool_call = true;
        let mut out = vec![StreamEvent::ToolCallStart {
            id: call_id.to_string(),
            name: name.to_string(),
            format,
        }];
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                id: call_id.to_string(),
                input_delta: args.to_string(),
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

    fn on_terminal(
        &mut self,
        v: &Value,
        event_type: &str,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let response = v.get("response");
        let declared_status = response
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str);
        let status = declared_status.unwrap_or(match event_type {
            "response.incomplete" => "incomplete",
            _ => "completed",
        });
        let expected = match event_type {
            "response.completed" => Some("completed"),
            "response.incomplete" => Some("incomplete"),
            _ => None,
        };
        if let Some(expected) = expected
            && status != expected
        {
            return Err(ProviderError::Decode(format!(
                "contradictory terminal: {event_type} carries status {status}"
            )));
        }
        let end_turn = match response.and_then(|r| r.get("end_turn")) {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => {
                return Err(ProviderError::Decode(
                    "response.end_turn must be a boolean".into(),
                ));
            }
        };
        if status == "incomplete" && end_turn.is_some() {
            return Err(ProviderError::Decode(
                "incomplete response must not carry end_turn".into(),
            ));
        }

        let mut out = Vec::new();
        if let Some(usage) = response.and_then(|r| r.get("usage")).and_then(parse_usage) {
            out.push(StreamEvent::Usage { usage });
        }
        out.push(StreamEvent::Done {
            stop: self.stop_for(status, end_turn, response)?,
        });
        Ok(out)
    }

    fn stop_for(
        &self,
        status: &str,
        end_turn: Option<bool>,
        response: Option<&Value>,
    ) -> Result<StopReason, ProviderError> {
        if self.saw_tool_call && status == "completed" {
            return Ok(StopReason::ToolUse);
        }
        match status {
            "completed" => Ok(if end_turn.unwrap_or(true) {
                StopReason::EndTurn
            } else {
                StopReason::Continue
            }),
            "incomplete" => {
                let reason = response
                    .and_then(|value| value.get("incomplete_details"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str);
                Ok(match reason {
                    Some("max_output_tokens") | Some("max_tokens") => StopReason::MaxTokens,
                    Some("content_filter") => StopReason::ContentFilter,
                    _ => StopReason::IncompleteUnknown,
                })
            }
            "failed" | "cancelled" => Ok(StopReason::Refusal),
            other => Err(ProviderError::Decode(format!(
                "unknown terminal status {other}"
            ))),
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

/// Call format of an output item, `None` when the item is not a tool call.
fn call_format(item: &Value) -> Option<ToolCallFormat> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Some(ToolCallFormat::Json),
        Some("custom_tool_call") => Some(ToolCallFormat::Text),
        _ => None,
    }
}

/// Reports an output item this mapper cannot read, and nothing at all for the
/// types it handles elsewhere (`message` travels through its own text deltas,
/// `reasoning` is captured before this point). An item with no readable `type`
/// is reported under a placeholder rather than ignored: a malformed item is
/// still an item we dropped.
fn unmapped_item_event(item: &Value) -> Vec<StreamEvent> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<untyped>");
    if MAPPED_OUTPUT_ITEM_TYPES.contains(&item_type) {
        return Vec::new();
    }
    vec![StreamEvent::UnmappedItem {
        item_type: item_type.to_string(),
    }]
}

/// Input carried by an opening item: `arguments` for a function call,
/// `input` for a custom one. Never mixed, so a freeform payload can never be
/// read back as JSON arguments.
fn initial_call_input(item: &Value, format: ToolCallFormat) -> &str {
    terminal_call_input(item, format).unwrap_or_default()
}

fn terminal_call_input(item: &Value, format: ToolCallFormat) -> Option<&str> {
    let field = match format {
        ToolCallFormat::Json => "arguments",
        ToolCallFormat::Text => "input",
    };
    item.get(field).and_then(Value::as_str)
}

/// `response.usage` -> `TokenUsage`. `input_tokens` includes the cached ones (we keep the
/// full context size for the compaction threshold, ARCHITECTURE 3.3).
///
/// The breakdown lives in the `*_tokens_details` sub-objects, exactly where the
/// Responses API puts it (baseline: `codex-rs/codex-api/src/sse/responses.rs:131`).
/// Each one is optional: a backend that reports only the two totals still yields
/// a valid usage, with a zeroed breakdown.
fn parse_usage(usage: &Value) -> Option<TokenUsage> {
    let input = usage.get("input_tokens").and_then(Value::as_u64)? as u32;
    let output = usage.get("output_tokens").and_then(Value::as_u64)? as u32;
    Some(TokenUsage {
        input,
        cached_input: detail(usage, "input_tokens_details", "cached_tokens"),
        cache_write_input: detail(usage, "input_tokens_details", "cache_write_tokens"),
        output,
        reasoning_output: detail(usage, "output_tokens_details", "reasoning_tokens"),
        total: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default() as u32,
    })
}

/// One counter of a `*_tokens_details` sub-object. Absent or malformed reads as
/// zero: a missing breakdown must never invalidate the totals that carry the
/// budget.
fn detail(usage: &Value, group: &str, field: &str) -> u32 {
    usage
        .get(group)
        .and_then(|group| group.get(field))
        .and_then(Value::as_u64)
        .unwrap_or_default() as u32
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
            usage: TokenUsage::new(120, 8)
        }));
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::EndTurn
            })
        );
    }

    #[test]
    fn completed_end_turn_false_requests_continuation() {
        let ev = ingest_all(&[
            r#"{"type":"response.completed","response":{"status":"completed","end_turn":false}}"#,
        ]);
        assert_eq!(
            ev,
            [StreamEvent::Done {
                stop: StopReason::Continue
            }]
        );
    }

    #[test]
    fn missing_end_turn_keeps_legacy_end_turn_behavior() {
        let ev =
            ingest_all(&[r#"{"type":"response.completed","response":{"status":"completed"}}"#]);
        assert_eq!(
            ev,
            [StreamEvent::Done {
                stop: StopReason::EndTurn
            }]
        );
    }

    #[test]
    fn incomplete_reasons_are_distinct() {
        for (reason, expected) in [
            ("max_output_tokens", StopReason::MaxTokens),
            ("content_filter", StopReason::ContentFilter),
            ("future_reason", StopReason::IncompleteUnknown),
        ] {
            let event = format!(
                r#"{{"type":"response.incomplete","response":{{"status":"incomplete","incomplete_details":{{"reason":"{reason}"}}}}}}"#
            );
            let ev = ingest_all(&[&event]);
            assert_eq!(ev.last(), Some(&StreamEvent::Done { stop: expected }));
        }
    }

    #[test]
    fn invalid_end_turn_and_contradictory_terminals_fail_closed() {
        for event in [
            r#"{"type":"response.completed","response":{"status":"completed","end_turn":"false"}}"#,
            r#"{"type":"response.completed","response":{"status":"incomplete"}}"#,
            r#"{"type":"response.incomplete","response":{"status":"incomplete","end_turn":false}}"#,
        ] {
            let mut mapper = CodexEventMapper::new();
            assert!(matches!(
                mapper.ingest(event),
                Err(ProviderError::Decode(_))
            ));
        }
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

        assert!(ev.contains(&StreamEvent::tool_call_start("call_7", "bash")));
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

        // invariant: concatenated input_delta = valid JSON.
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, input_delta } if id == "call_7" => {
                    Some(input_delta.clone())
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
                    StreamEvent::ToolCallDelta { id, input_delta } if id == call_id => {
                        Some(input_delta.clone())
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
                StreamEvent::ToolCallDelta { id, input_delta }
                if id == "call_b" && input_delta == "{\"path\":\"b.txt\"}"
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
                StreamEvent::ToolCallDelta { input_delta, .. } => Some(input_delta.clone()),
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
                StreamEvent::ToolCallStart { id, name, .. } if id == "c1" && name == "read"
            )
        }));
        assert!(ev.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallDelta { id, input_delta }
                if id == "c1" && input_delta == "{\"path\":\"Cargo.toml\"}"
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
    fn incomplete_status_without_a_reason_fails_closed_as_unknown() {
        let ev =
            ingest_all(&[r#"{"type":"response.incomplete","response":{"status":"incomplete"}}"#]);
        assert_eq!(
            ev,
            vec![StreamEvent::Done {
                stop: StopReason::IncompleteUnknown
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

    #[test]
    fn fragmented_custom_tool_call_yields_one_terminal_call_with_text_input() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":""}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"// @exec: cell\n"}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"const x = 1;"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":"// @exec: cell\nconst x = 1;"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert_eq!(
            ev,
            vec![
                StreamEvent::custom_tool_call_start("call_1", "exec"),
                StreamEvent::ToolCallDelta {
                    id: "call_1".into(),
                    input_delta: "// @exec: cell\nconst x = 1;".into(),
                },
                StreamEvent::ToolCallEnd {
                    id: "call_1".into()
                },
                StreamEvent::Done {
                    stop: StopReason::ToolUse
                },
            ]
        );
    }

    /// Duplicated deltas then an authoritative terminal item: the terminal
    /// input wins and nothing is emitted twice.
    #[test]
    fn duplicate_deltas_lose_against_the_terminal_item() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":""}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"print(1)"}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"print(1)"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":"print(1)"}}"#,
        ]);
        let deltas: Vec<&str> = ev
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta { input_delta, .. } => Some(input_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["print(1)"], "terminal item is authoritative");
        assert_eq!(
            ev.iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                .count(),
            1,
            "one dispatch, not two"
        );
    }

    #[test]
    fn custom_tool_call_done_without_added_is_reconstructed() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"c1","name":"apply_patch","input":"*** Begin Patch"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert_eq!(
            ev.first(),
            Some(&StreamEvent::custom_tool_call_start("c1", "apply_patch"))
        );
        assert!(ev.contains(&StreamEvent::ToolCallDelta {
            id: "c1".into(),
            input_delta: "*** Begin Patch".into(),
        }));
    }

    #[test]
    fn impossible_custom_streams_fail_closed_without_dispatch() {
        // Terminal item without a name: nothing is dispatchable.
        let mut m = CodexEventMapper::new();
        assert!(matches!(
            m.ingest(
                r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"c1","input":"x"}}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

        // A custom delta addressed to a function call would silently corrupt
        // its JSON arguments, and the reverse would corrupt freeform text.
        let mut m = CodexEventMapper::new();
        m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
        )
        .unwrap();
        assert!(matches!(
            m.ingest(
                r#"{"type":"response.custom_tool_call_input.delta","item_id":"fc_a","delta":"raw"}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

        let mut m = CodexEventMapper::new();
        m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_a","name":"exec","input":""}}"#,
        )
        .unwrap();
        assert!(matches!(
            m.ingest(
                r#"{"type":"response.function_call_arguments.delta","item_id":"ctc_a","delta":"{}"}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

        // Format flip between the opening and the terminal item.
        let mut m = CodexEventMapper::new();
        m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"c2","id":"ctc_2","name":"exec","input":""}}"#,
        )
        .unwrap();
        assert!(matches!(
            m.ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","id":"ctc_2","name":"exec","arguments":"{}"}}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

        // Invalid UTF-8 escape inside the payload: rejected at decode, so no
        // call is ever built from it.
        let mut m = CodexEventMapper::new();
        assert!(matches!(
            m.ingest(
                "{\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"c3\",\"name\":\"exec\",\"input\":\"\\ud800\"}}",
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));
    }

    #[test]
    fn function_and_custom_calls_interleave_without_crosstalk() {
        let ev = ingest_all(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_f","id":"fc_1","name":"read","arguments":""}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_1","name":"exec","input":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"a.txt\"}"}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"await tools.read()"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_f","id":"fc_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_1","name":"exec","input":"await tools.read()"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        let payload = |call: &str| -> Option<String> {
            ev.iter().find_map(|event| match event {
                StreamEvent::ToolCallDelta { id, input_delta } if id == call => {
                    Some(input_delta.clone())
                }
                _ => None,
            })
        };
        assert_eq!(payload("call_f").as_deref(), Some(r#"{"path":"a.txt"}"#));
        assert_eq!(payload("call_c").as_deref(), Some("await tools.read()"));
        assert!(ev.contains(&StreamEvent::tool_call_start("call_f", "read")));
        assert!(ev.contains(&StreamEvent::custom_tool_call_start("call_c", "exec")));
    }
}
