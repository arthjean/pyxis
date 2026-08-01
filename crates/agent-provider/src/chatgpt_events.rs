//! Mapping of the SSE events of the Responses API (ChatGPT/Codex backend) into the
//! canonical `StreamEvent` vocabulary (PROVIDERS 2). Stateful: tracks the
//! active function calls, emits fragments at arrival time, then publishes the
//! authoritative terminal input before `ToolCallEnd`.
//!
//! Known semantic events receive dedicated canonical variants. Other event and
//! item types remain observable as bounded, sanitized provider extensions.

use agent_core::message::ToolCallFormat;
use agent_core::provider::{
    ProviderError, ProviderErrorCategory, ProviderExtension, ReasoningMetadata, ResponseMetadata,
    StopReason, StreamEvent, TokenUsage,
};
use serde_json::Value;
use std::collections::HashMap;

use crate::chatgpt_error::{failed_error, incomplete_error, stream_error, terminal_error};
use crate::chatgpt_metadata::response_metadata_from_event;

struct ActiveCall {
    call_id: String,
    args: String,
    format: ToolCallFormat,
}

/// Responses output item types this mapper knowingly handles: projected here
/// (`function_call`, `custom_tool_call`, `reasoning`) or carried by their own
/// delta events (`message`). The conformance suite reads it so an item the
/// wire starts sending fails a test instead of disappearing from the stream.
pub const MAPPED_OUTPUT_ITEM_TYPES: &[&str] =
    &["message", "reasoning", "function_call", "custom_tool_call"];

/// Baseline lifecycle events preserved as extensions because the canonical
/// vocabulary has no dedicated projection for them.
const KNOWN_UNPROJECTED_EVENTS: &[&str] = &[
    "response.in_progress",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.done",
    "response.reasoning_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_text.done",
];

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
    terminal: bool,
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
    /// `StreamEvent`. Only `response.completed` emits `Usage?` then `Done`.
    /// Incomplete, failed, legacy done and explicit error events surface a typed
    /// `ProviderError`, never a panic.
    pub fn ingest(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.terminal {
            return Err(ProviderError::Decode(
                "Responses event received after terminal success".into(),
            ));
        }
        let data = data.trim();
        if data.is_empty() {
            return Ok(vec![malformed_event(0, "empty_payload")]);
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => return Ok(vec![malformed_event(data.len(), "invalid_json")]),
        };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");

        let events = match typ {
            "response.created" | "response.metadata" => Ok(vec![extension_event(typ, &v)]),
            "response.output_text.delta" => {
                Ok(delta_event(&v, |text| StreamEvent::TextDelta { text }))
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let mut events = vec![extension_event(typ, &v)];
                events.extend(delta_event(&v, |text| StreamEvent::ReasoningDelta { text }));
                Ok(events)
            }
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => Ok(vec![extension_event(typ, &v)]),
            "response.output_item.added" => Ok(self.on_item_added(&v)),
            "response.function_call_arguments.delta" => {
                if let (Some(key), Some(delta)) = (
                    self.event_item_key(&v, "function_call_arguments.delta")?,
                    v.get("delta").and_then(Value::as_str),
                ) {
                    if let Some(active) = self.active.get_mut(&key) {
                        if active.format != ToolCallFormat::Json {
                            return Err(ProviderError::Decode(format!(
                                "function arguments delta on custom tool call {}",
                                active.call_id
                            )));
                        }
                        active.args.push_str(delta);
                        Ok(vec![StreamEvent::ToolCallDelta {
                            id: active.call_id.clone(),
                            input_delta: delta.to_string(),
                        }])
                    } else {
                        Ok(vec![extension_event(typ, &v)])
                    }
                } else {
                    Ok(vec![extension_event(typ, &v)])
                }
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
                Ok(vec![extension_event(typ, &v)])
            }
            // Freeform input arrives as text fragments on its own event name.
            "response.custom_tool_call_input.delta" => {
                if let (Some(key), Some(delta)) = (
                    self.event_item_key(&v, "custom_tool_call_input.delta")?,
                    v.get("delta").and_then(Value::as_str),
                ) {
                    if let Some(active) = self.active.get_mut(&key) {
                        if active.format != ToolCallFormat::Text {
                            return Err(ProviderError::Decode(format!(
                                "custom tool input delta on function call {}",
                                active.call_id
                            )));
                        }
                        active.args.push_str(delta);
                        Ok(vec![StreamEvent::ToolCallDelta {
                            id: active.call_id.clone(),
                            input_delta: delta.to_string(),
                        }])
                    } else {
                        Ok(vec![extension_event(typ, &v)])
                    }
                } else {
                    Ok(vec![extension_event(typ, &v)])
                }
            }
            "response.custom_tool_call_input.done" => {
                if let (Some(key), Some(input)) = (
                    self.event_item_key(&v, "custom_tool_call_input.done")?,
                    v.get("input").and_then(Value::as_str),
                ) && let Some(active) = self.active.get_mut(&key)
                {
                    active.args = input.to_string();
                }
                Ok(vec![extension_event(typ, &v)])
            }
            "response.output_item.done" => self.on_item_done(&v),
            "response.completed" => {
                let events = self.on_completed(&v)?;
                self.terminal = true;
                Ok(events)
            }
            "response.done" => Err(terminal_error(
                ProviderErrorCategory::Failed,
                Some(503),
                "response.done is not a successful Responses terminal",
                &v,
            )),
            "response.incomplete" => Err(incomplete_error(&v)),
            // Quota state pushed mid-stream. Richer than the response headers,
            // which never name the plan, so it is read here as well.
            "codex.rate_limits" => Ok(crate::quota::parse_quota_event(&v)
                .map(|snapshot| vec![StreamEvent::Quota { snapshot }])
                .unwrap_or_default()),
            "error" => Err(stream_error(&v)),
            "response.failed" => Err(failed_error(&v)),
            known if KNOWN_UNPROJECTED_EVENTS.contains(&known) => {
                Ok(vec![extension_event(known, &v)])
            }
            // A baseline event not yet represented by a dedicated canonical
            // variant remains visible as bounded, redacted data. This wildcard
            // must never become a silent drop again.
            _ => Ok(vec![StreamEvent::ProviderExtension {
                extension: ProviderExtension::from_value(
                    if typ.is_empty() {
                        "<untyped-event>"
                    } else {
                        typ
                    },
                    v.clone(),
                ),
            }]),
        };
        let mut events = events?;
        let metadata = response_metadata_from_event(&v);
        if !metadata.is_empty() {
            events.insert(
                0,
                StreamEvent::ResponseMetadata {
                    metadata: Box::new(metadata),
                },
            );
        }
        Ok(events)
    }

    fn on_item_added(&mut self, v: &Value) -> Vec<StreamEvent> {
        let item = match v.get("item") {
            Some(i) => i,
            None => return vec![extension_event("response.output_item.added", v)],
        };
        let Some(format) = call_format(item) else {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|item_type| MAPPED_OUTPUT_ITEM_TYPES.contains(&item_type))
            {
                return vec![extension_event("response.output_item.added", v)];
            }
            return unmapped_item_event(item, "added");
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
        vec![
            extension_event("response.output_item.added", v),
            StreamEvent::ToolCallStart {
                id: call_id,
                name,
                format,
            },
        ]
    }

    fn on_item_done(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let item_type = v
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str);
        // US-031: encrypted reasoning item, captured only when replay is active.
        // `encrypted_content`/`id` opaque.
        if item_type == Some("reasoning") {
            let item = match v.get("item") {
                Some(i) => i,
                None => return Ok(Vec::new()),
            };
            let mut events = vec![
                extension_event("response.output_item.done", v),
                StreamEvent::ResponseMetadata {
                    metadata: Box::new(ResponseMetadata {
                        reasoning: ReasoningMetadata {
                            item_id: item_id(item).map(str::to_string),
                            status: item
                                .get("status")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            ..ReasoningMetadata::default()
                        },
                        ..ResponseMetadata::default()
                    }),
                },
            ];
            if !self.replay {
                return Ok(events);
            }
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let enc = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // a reasoning without encrypted content is not reinjectable -> ignored.
            if id.is_empty() || enc.is_empty() {
                return Ok(events);
            }
            events.push(StreamEvent::EncryptedReasoning {
                id: id.to_string(),
                encrypted_content: enc.to_string(),
            });
            return Ok(events);
        }
        let Some(item) = v.get("item") else {
            return Ok(vec![extension_event("response.output_item.done", v)]);
        };
        let Some(format) = call_format(item) else {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|item_type| MAPPED_OUTPUT_ITEM_TYPES.contains(&item_type))
            {
                return Ok(vec![extension_event("response.output_item.done", v)]);
            }
            // Everything outside the mapped set is content we cannot read. It is
            // REPORTED rather than dropped: a web search or an image generation
            // served by the backend would otherwise leave no trace at all.
            return Ok(unmapped_item_event(item, "done"));
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
        let mut out = vec![extension_event("response.output_item.done", v)];
        // Deltas were emitted at arrival time. The terminal item is carried as
        // an authoritative replacement, never as a duplicate delta.
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallInputDone {
                id: active.call_id.clone(),
                input: args,
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
            out.push(StreamEvent::ToolCallInputDone {
                id: call_id.to_string(),
                input: args.to_string(),
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

    fn on_completed(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let response = v.get("response");
        if response
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
        {
            return Err(ProviderError::Decode(
                "response.completed is missing response.id".into(),
            ));
        }
        let declared_status = response
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str);
        let status = declared_status.unwrap_or("completed");
        if status != "completed" {
            return Err(ProviderError::Decode(format!(
                "contradictory terminal: response.completed carries status {status}"
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
        let mut out = Vec::new();
        if let Some(usage) = response
            .and_then(|r| r.get("usage"))
            .map(parse_usage)
            .transpose()?
        {
            out.push(StreamEvent::Usage { usage });
        }
        out.push(StreamEvent::Done {
            stop: self.stop_for_completed(end_turn),
        });
        Ok(out)
    }

    fn stop_for_completed(&self, end_turn: Option<bool>) -> StopReason {
        if self.saw_tool_call {
            StopReason::ToolUse
        } else if end_turn.unwrap_or(true) {
            StopReason::EndTurn
        } else {
            StopReason::Continue
        }
    }
}

fn extension_event(event_type: &str, value: &Value) -> StreamEvent {
    StreamEvent::ProviderExtension {
        extension: ProviderExtension::from_value(event_type, value.clone()),
    }
}

fn malformed_event(original_bytes: usize, reason: &str) -> StreamEvent {
    StreamEvent::ProviderExtension {
        extension: ProviderExtension::from_value(
            "malformed_sse_event_ignored",
            serde_json::json!({
                "original_bytes": original_bytes,
                "reason": reason,
            }),
        ),
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
fn unmapped_item_event(item: &Value, phase: &str) -> Vec<StreamEvent> {
    let raw_item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<untyped>");
    let item_type: String = raw_item_type.chars().take(128).collect();
    let item_type = if item_type.is_empty()
        || !item_type.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        }) {
        "<invalid-type>".to_string()
    } else {
        item_type
    };
    vec![StreamEvent::UnmappedItem {
        item_type: item_type.clone(),
        extension: Some(ProviderExtension::from_value(
            format!("response.item.{item_type}.{phase}"),
            item.clone(),
        )),
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
fn parse_usage(usage: &Value) -> Result<TokenUsage, ProviderError> {
    let input = counter(usage, "input_tokens")?;
    let output = counter(usage, "output_tokens")?;
    let total = match usage.get("total_tokens") {
        Some(value) => non_negative_counter(value, "total_tokens")?,
        None => 0,
    };
    Ok(TokenUsage {
        input,
        cached_input: detail(usage, "input_tokens_details", "cached_tokens")?,
        cache_write_input: detail(usage, "input_tokens_details", "cache_write_tokens")?,
        output,
        reasoning_output: detail(usage, "output_tokens_details", "reasoning_tokens")?,
        total,
    })
}

/// One counter of a `*_tokens_details` sub-object. Absent or malformed reads as
/// zero: a missing breakdown must never invalidate the totals that carry the
/// budget.
fn detail(usage: &Value, group: &str, field: &str) -> Result<u64, ProviderError> {
    let Some(value) = usage.get(group).and_then(|group| group.get(field)) else {
        return Ok(0);
    };
    non_negative_counter(value, &format!("{group}.{field}"))
}

fn counter(usage: &Value, field: &str) -> Result<u64, ProviderError> {
    let value = usage
        .get(field)
        .ok_or_else(|| ProviderError::Decode(format!("response usage {field} is missing")))?;
    non_negative_counter(value, field)
}

fn non_negative_counter(value: &Value, field: &str) -> Result<u64, ProviderError> {
    let value = value.as_i64().ok_or_else(|| {
        ProviderError::Decode(format!("response usage {field} is outside the i64 range"))
    })?;
    u64::try_from(value)
        .map_err(|_| ProviderError::Decode(format!("response usage {field} must be non-negative")))
}

#[cfg(test)]
mod tests;
