//! Pure construction and capture of turn-scoped incremental Responses bodies.

use agent_core::provider::ProviderError;
use serde_json::{Map, Value};

use crate::chatgpt_metadata::turn_state_from_event;

const MAX_INCREMENTAL_ITEMS: usize = 4096;
const MAX_INCREMENTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_ID_BYTES: usize = 1024;
const MAX_TURN_STATE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestMode {
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ContinuationInput<'a> {
    Full,
    Incremental {
        previous_response_id: &'a str,
        input: &'a [Value],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GenerationMode {
    Generate,
    Prewarm,
}

impl RequestMode {
    pub(super) fn is_incremental(self) -> bool {
        self == Self::Incremental
    }
}

#[derive(Clone)]
struct RequestShape {
    properties: Map<String, Value>,
    input: Vec<Value>,
}

impl RequestShape {
    fn from_body(body: &Value) -> Option<Self> {
        let mut properties = body.as_object()?.clone();
        let input = properties.remove("input")?.as_array()?.clone();
        for volatile in ["stream_options", "type", "previous_response_id", "generate"] {
            properties.remove(volatile);
        }
        if let Some(metadata) = properties
            .get_mut("client_metadata")
            .and_then(Value::as_object_mut)
        {
            metadata.remove("x-codex-turn-state");
            metadata.remove("x-codex-ws-stream-request-start-ms");
        }
        Some(Self { properties, input })
    }
}

#[derive(Clone)]
pub(super) struct ContinuationState {
    request: RequestShape,
    response_id: String,
    output_items: Vec<Value>,
}

#[derive(Default)]
pub(super) struct ResponseCapture {
    response_id: Option<String>,
    output_items: Vec<Value>,
    output_items_bytes: usize,
}

impl ResponseCapture {
    pub(super) fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    pub(super) fn into_continuation(self, full_body: &Value) -> Option<ContinuationState> {
        let request = RequestShape::from_body(full_body)?;
        Some(ContinuationState {
            request,
            response_id: self.response_id?,
            output_items: self.output_items,
        })
    }
}

pub(super) fn prepare_wire_body(
    full: &Value,
    previous: Option<&ContinuationState>,
    turn_state: Option<&str>,
) -> (Value, RequestMode) {
    let Some(previous) = previous else {
        return (
            response_create_body(
                full,
                ContinuationInput::Full,
                turn_state,
                GenerationMode::Generate,
            ),
            RequestMode::Full,
        );
    };
    let Some(current) = RequestShape::from_body(full) else {
        return (
            response_create_body(
                full,
                ContinuationInput::Full,
                turn_state,
                GenerationMode::Generate,
            ),
            RequestMode::Full,
        );
    };
    let Some(suffix) = strict_suffix(&current, previous) else {
        return (
            response_create_body(
                full,
                ContinuationInput::Full,
                turn_state,
                GenerationMode::Generate,
            ),
            RequestMode::Full,
        );
    };
    (
        response_create_body(
            full,
            ContinuationInput::Incremental {
                previous_response_id: &previous.response_id,
                input: &suffix,
            },
            turn_state,
            GenerationMode::Generate,
        ),
        RequestMode::Incremental,
    )
}

pub(super) fn response_create_body(
    full: &Value,
    continuation: ContinuationInput<'_>,
    turn_state: Option<&str>,
    generation: GenerationMode,
) -> Value {
    let mut body = full.clone();
    body["type"] = Value::String("response.create".into());
    match continuation {
        ContinuationInput::Full => {
            if let Some(object) = body.as_object_mut() {
                object.remove("previous_response_id");
            }
        }
        ContinuationInput::Incremental {
            previous_response_id,
            input,
        } => {
            body["previous_response_id"] = Value::String(previous_response_id.to_string());
            body["input"] = Value::Array(input.to_vec());
        }
    }
    match generation {
        GenerationMode::Generate => {
            if let Some(object) = body.as_object_mut() {
                object.remove("generate");
            }
        }
        GenerationMode::Prewarm => body["generate"] = Value::Bool(false),
    }
    if let Some(turn_state) = turn_state {
        if !body["client_metadata"].is_object() {
            body["client_metadata"] = serde_json::json!({});
        }
        body["client_metadata"]["x-codex-turn-state"] = Value::String(turn_state.to_string());
    }
    body
}

pub(super) fn capture_response_state(
    event: &Value,
    capture: &mut ResponseCapture,
    turn_state: &mut Option<String>,
) -> Result<(), ProviderError> {
    if let Some(state) = turn_state_from_event(event) {
        *turn_state = Some(validate_turn_state(&state)?);
    }
    if event.get("type").and_then(Value::as_str) == Some("response.output_item.done")
        && let Some(item) = event.get("item")
    {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| ProviderError::Decode("websocket output item is invalid".into()))?
            .len();
        if capture.output_items.len() >= MAX_INCREMENTAL_ITEMS
            || capture.output_items_bytes.saturating_add(item_bytes) > MAX_INCREMENTAL_BYTES
        {
            return Err(ProviderError::Decode(
                "websocket incremental baseline exceeds its safe bound".into(),
            ));
        }
        capture.output_items_bytes += item_bytes;
        capture.output_items.push(item.clone());
    }
    if event.get("type").and_then(Value::as_str) == Some("response.completed")
        && let Some(id) = event
            .get("response")
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
    {
        if id.is_empty() || id.len() > MAX_RESPONSE_ID_BYTES || id.chars().any(char::is_control) {
            return Err(ProviderError::Decode(
                "websocket response ID exceeds its safe bound".into(),
            ));
        }
        capture.response_id = Some(id.to_string());
    }
    Ok(())
}

pub(super) fn validate_turn_state(state: &str) -> Result<String, ProviderError> {
    if state.len() > MAX_TURN_STATE_BYTES || state.chars().any(char::is_control) {
        return Err(ProviderError::Decode(
            "websocket turn state exceeds its safe bound".into(),
        ));
    }
    Ok(state.to_string())
}

pub(super) fn previous_response_not_found(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("error")
        && event
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            == Some("previous_response_not_found")
}

fn strict_suffix(current: &RequestShape, previous: &ContinuationState) -> Option<Vec<Value>> {
    if previous.request.properties != current.properties {
        return None;
    }
    let baseline_len = previous
        .request
        .input
        .len()
        .checked_add(previous.output_items.len())?;
    if current.input.len() <= baseline_len {
        return None;
    }
    let baseline = previous.request.input.iter().chain(&previous.output_items);
    if !baseline
        .zip(&current.input[..baseline_len])
        .all(|(left, right)| normalized_item(left) == normalized_item(right))
    {
        return None;
    }
    Some(current.input[baseline_len..].to_vec())
}

fn normalized_item(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.remove("id");
        object.remove("status");
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(input: Vec<Value>) -> Value {
        serde_json::json!({
            "model": "gpt-5.5",
            "instructions": "code",
            "input": input,
            "tools": [],
            "stream": true,
            "store": false,
            "client_metadata": { "turn_id": "turn-1" }
        })
    }

    fn continuation(
        full: &Value,
        response_id: &str,
        output_items: Vec<Value>,
    ) -> ContinuationState {
        ContinuationState {
            request: RequestShape::from_body(full).expect("test body has an input array"),
            response_id: response_id.into(),
            output_items,
        }
    }

    #[test]
    fn incremental_request_sends_only_a_strict_suffix() {
        let user = serde_json::json!({"type":"message","role":"user","content":[]});
        let assistant = serde_json::json!({
            "type":"message", "id":"msg_1", "status":"completed",
            "role":"assistant", "content":[]
        });
        let normalized_assistant = serde_json::json!({
            "type":"message", "role":"assistant", "content":[]
        });
        let tool_result =
            serde_json::json!({"type":"function_call_output","call_id":"c","output":"ok"});
        let previous = body(vec![user.clone()]);
        let current = body(vec![user, normalized_assistant, tool_result.clone()]);
        let previous = continuation(&previous, "resp_1", vec![assistant]);

        let (wire, mode) = prepare_wire_body(&current, Some(&previous), Some("sticky"));

        assert_eq!(mode, RequestMode::Incremental);
        assert_eq!(wire["previous_response_id"], "resp_1");
        assert_eq!(wire["input"], serde_json::json!([tool_result]));
        assert_eq!(wire["client_metadata"]["x-codex-turn-state"], "sticky");
    }

    #[test]
    fn changed_properties_or_missing_response_id_force_a_full_request() {
        let user = serde_json::json!({"type":"message","role":"user","content":[]});
        let previous_body = body(vec![user.clone()]);
        let previous = continuation(&previous_body, "resp_1", Vec::new());
        let mut current = body(vec![user.clone(), user]);
        current["model"] = Value::String("gpt-5.4".into());

        let (wire, mode) = prepare_wire_body(&current, Some(&previous), None);
        assert_eq!(mode, RequestMode::Full);
        assert!(wire.get("previous_response_id").is_none());

        let capture = ResponseCapture::default();
        assert!(capture.into_continuation(&previous_body).is_none());
    }

    #[test]
    fn capture_replaces_changed_turn_state_and_bounds_incremental_state() {
        let mut capture = ResponseCapture::default();
        let mut turn_state = Some("old".into());
        capture_response_state(
            &serde_json::json!({"type":"response.metadata","turn_state":"new"}),
            &mut capture,
            &mut turn_state,
        )
        .expect("bounded turn state");
        assert_eq!(turn_state.as_deref(), Some("new"));

        let oversized_id = "r".repeat(MAX_RESPONSE_ID_BYTES + 1);
        assert!(
            capture_response_state(
                &serde_json::json!({"type":"response.completed","response":{"id":oversized_id}}),
                &mut capture,
                &mut turn_state,
            )
            .is_err()
        );
    }
}
