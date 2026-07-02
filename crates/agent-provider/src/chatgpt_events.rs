//! Mapping des events SSE de la Responses API (backend ChatGPT/Codex) vers le
//! vocabulaire canonique `StreamEvent` (PROVIDERS §2). Stateful : suit le
//! function call actif et accumule ses arguments pour garantir l'invariant
//! « `args_json` complet & valide à `ToolCallEnd` ».
//!
//! Types d'events transcrits verbatim de Pi (`openai-responses-shared.ts` +
//! `openai-codex-responses.ts`). Les events non pertinents (created, part.added,
//! content_part.added…) sont silencieusement ignorés — comme Pi.

use agent_core::provider::{ProviderError, StopReason, StreamEvent, TokenUsage};
use serde_json::Value;
use std::collections::HashMap;

struct ActiveCall {
    call_id: String,
    args: String,
}

/// Mapper à état pour un flux de réponse. Réinstancié à chaque tour.
#[derive(Default)]
pub struct CodexEventMapper {
    active: HashMap<String, ActiveCall>,
    output_index_to_item: HashMap<u64, String>,
    last_active_item: Option<String>,
    /// Au moins un tool call a-t-il été émis ? (override stop `completed`→`ToolUse`).
    saw_tool_call: bool,
    /// US-031 : capturer les reasoning items chiffrés pour replay ? Le mapper brut
    /// garde un défaut OFF ; le provider ChatGPT l'active explicitement.
    replay: bool,
}

impl CodexEventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construit un mapper avec le replay reasoning (US-031) activé ou non.
    pub fn with_replay(replay: bool) -> Self {
        Self {
            replay,
            ..Self::default()
        }
    }

    /// Traduit un payload `data:` SSE (un event Responses JSON) en 0..n
    /// `StreamEvent`. Un event terminal (`response.completed`/`.done`/
    /// `.incomplete`) émet `Usage?` puis `Done`. Une erreur (`error`/
    /// `response.failed`) remonte une `ProviderError` typée — jamais de panic.
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
                // Source d'autorité des arguments complets (remplace l'accumulé).
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
            // created, content_part.added, reasoning_summary_part.added, … → ignorés.
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
        // arguments souvent "" à l'ouverture ; on accumule la suite.
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
        // US-031 : reasoning item chiffré, capturé uniquement si replay actif.
        // `encrypted_content`/`id` opaques.
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
            // un reasoning sans contenu chiffré n'est pas réinjectable → ignoré.
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
        // arguments finaux : priorité à l'item.done, sinon l'accumulé.
        let item_args = v
            .get("item")
            .and_then(|i| i.get("arguments"))
            .and_then(Value::as_str);
        let Some(key) = self.event_item_key(v, "output_item.done")? else {
            return Ok(Vec::new());
        };
        let Some(active) = self.active.remove(&key) else {
            return Ok(Vec::new());
        };
        if self.last_active_item.as_deref() == Some(key.as_str()) {
            self.last_active_item = None;
        }
        let args = match item_args {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => active.args,
        };
        let mut out = Vec::new();
        // Un seul ToolCallDelta portant l'intégralité → invariant JSON garanti.
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                id: active.call_id.clone(),
                args_json: args,
            });
        }
        out.push(StreamEvent::ToolCallEnd { id: active.call_id });
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
            // completed / in_progress / queued / absent → fin normale ;
            // override ToolUse si des appels d'outils ont été émis.
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

/// `response.usage` → `TokenUsage`. `input_tokens` inclut les cached (on garde la
/// taille de contexte complète pour le seuil de compaction, ARCHITECTURE §3.3).
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
    classify_message(code, message)
}

/// Distingue un dépassement de contexte (→ withholding/compaction réactive) d'une
/// erreur de flux générique.
fn classify_message(code: &str, message: &str) -> ProviderError {
    let hay = format!("{code} {message}").to_lowercase();
    if hay.contains("context") && (hay.contains("length") || hay.contains("long")) {
        ProviderError::ContextLengthExceeded
    } else {
        ProviderError::Stream(format!("{code}: {message}"))
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
        let ev = ingest_all(&[r#"{"type":"response.output_text.delta","delta":"Bonjour"}"#]);
        assert_eq!(
            ev,
            vec![StreamEvent::TextDelta {
                text: "Bonjour".into()
            }]
        );
    }

    #[test]
    fn reasoning_deltas_map() {
        let ev = ingest_all(&[
            r#"{"type":"response.reasoning_summary_text.delta","delta":"je réfléchis"}"#,
            r#"{"type":"response.reasoning_text.delta","delta":" encore"}"#,
        ]);
        assert_eq!(
            ev,
            vec![
                StreamEvent::ReasoningDelta {
                    text: "je réfléchis".into()
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
        // stop = ToolUse car un appel d'outil a été émis.
        assert_eq!(
            ev.last(),
            Some(&StreamEvent::Done {
                stop: StopReason::ToolUse
            })
        );

        // invariant : args_json concaténé = JSON valide.
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
        // backend qui n'envoie pas de deltas : args uniquement dans output_item.done.
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

    // US-031 : reasoning item chiffré capturé uniquement si replay actif.
    #[test]
    fn reasoning_item_captured_only_when_replay_on() {
        let done = r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC"}}"#;
        // OFF (défaut) → ignoré (chemin plat).
        assert!(CodexEventMapper::new().ingest(done).unwrap().is_empty());
        // ON → EncryptedReasoning émis.
        let ev = CodexEventMapper::with_replay(true).ingest(done).unwrap();
        assert_eq!(
            ev,
            vec![StreamEvent::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into()
            }]
        );
        // reasoning sans contenu chiffré → ignoré même en ON (non réinjectable).
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
        // ligne vide → no-op.
        assert!(m.ingest("").unwrap().is_empty());
    }
}
