//! Provider-neutral Responses output items.
//!
//! The item kind is typed so consumers can branch without inspecting provider
//! JSON. The complete provider payload stays bounded and redacted in a
//! `ProviderExtension`, preserving additive fields without importing OpenAI
//! wire structs into `agent-core`.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::provider::ProviderExtension;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseItemPhase {
    Added,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseItemKind {
    Message,
    AgentMessage,
    Reasoning,
    LocalShellCall,
    FunctionCall,
    FunctionCallOutput,
    CustomToolCall,
    CustomToolCallOutput,
    ToolSearchCall,
    ToolSearchOutput,
    WebSearchCall,
    ImageGenerationCall,
    Compaction,
    CompactionTrigger,
    ContextCompaction,
    Other(String),
}

impl ResponseItemKind {
    pub fn from_wire_type(value: &str) -> Self {
        match value {
            "message" => Self::Message,
            "agent_message" => Self::AgentMessage,
            "reasoning" => Self::Reasoning,
            "local_shell_call" => Self::LocalShellCall,
            "function_call" => Self::FunctionCall,
            "function_call_output" => Self::FunctionCallOutput,
            "custom_tool_call" => Self::CustomToolCall,
            "custom_tool_call_output" => Self::CustomToolCallOutput,
            "tool_search_call" => Self::ToolSearchCall,
            "tool_search_output" => Self::ToolSearchOutput,
            "web_search_call" => Self::WebSearchCall,
            "image_generation_call" => Self::ImageGenerationCall,
            "compaction" | "compaction_summary" => Self::Compaction,
            "compaction_trigger" => Self::CompactionTrigger,
            "context_compaction" => Self::ContextCompaction,
            other => Self::Other(other.chars().take(128).collect()),
        }
    }

    pub fn wire_type(&self) -> &str {
        match self {
            Self::Message => "message",
            Self::AgentMessage => "agent_message",
            Self::Reasoning => "reasoning",
            Self::LocalShellCall => "local_shell_call",
            Self::FunctionCall => "function_call",
            Self::FunctionCallOutput => "function_call_output",
            Self::CustomToolCall => "custom_tool_call",
            Self::CustomToolCallOutput => "custom_tool_call_output",
            Self::ToolSearchCall => "tool_search_call",
            Self::ToolSearchOutput => "tool_search_output",
            Self::WebSearchCall => "web_search_call",
            Self::ImageGenerationCall => "image_generation_call",
            Self::Compaction => "compaction",
            Self::CompactionTrigger => "compaction_trigger",
            Self::ContextCompaction => "context_compaction",
            Self::Other(wire_type) => wire_type,
        }
    }

    pub fn is_other(&self) -> bool {
        matches!(self, Self::Other(_))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    kind: ResponseItemKind,
    payload: ProviderExtension,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
}

impl ResponseItem {
    pub fn from_wire(value: &Value) -> Result<Self, ResponseItemError> {
        Self::from_wire_at_phase(value, ResponseItemPhase::Done)
    }

    pub fn from_wire_at_phase(
        value: &Value,
        phase: ResponseItemPhase,
    ) -> Result<Self, ResponseItemError> {
        let object = value.as_object().ok_or(ResponseItemError::NotObject)?;
        let wire_type = object
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(ResponseItemError::MissingType)?;
        validate_wire_type(wire_type)?;
        let kind = ResponseItemKind::from_wire_type(wire_type);
        if phase == ResponseItemPhase::Done {
            validate_known_payload(&kind, value)?;
        }
        let id = optional_bounded_string(object.get("id"), "id", 256)?;
        let status = optional_bounded_string(object.get("status"), "status", 128)?;

        let original_bytes = serde_json::to_vec(value)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(u64::MAX);
        let mut payload = value.clone();
        let mut pre_redacted = false;
        if kind == ResponseItemKind::ImageGenerationCall
            && let Some(result) = payload
                .as_object_mut()
                .and_then(|object| object.get_mut("result"))
        {
            *result = Value::String("[REDACTED_BINARY]".into());
            pre_redacted = true;
        }
        let diagnostic = kind
            .is_other()
            .then(|| "unknown response item type preserved as bounded Other".to_string());
        Ok(Self {
            id,
            status,
            kind,
            payload: if pre_redacted {
                ProviderExtension::from_redacted_value(
                    format!("response.item.{wire_type}"),
                    payload,
                    original_bytes,
                )
            } else {
                ProviderExtension::from_value(format!("response.item.{wire_type}"), payload)
            },
            diagnostic,
        })
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn kind(&self) -> &ResponseItemKind {
        &self.kind
    }

    pub fn payload(&self) -> &ProviderExtension {
        &self.payload
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }
}

#[derive(Deserialize)]
struct ResponseItemWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    kind: ResponseItemKind,
    payload: ProviderExtension,
    #[serde(default)]
    diagnostic: Option<String>,
}

impl<'de> Deserialize<'de> for ResponseItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResponseItemWire::deserialize(deserializer)?;
        validate_bounded_option(wire.id.as_deref(), "id", 256).map_err(serde::de::Error::custom)?;
        validate_bounded_option(wire.status.as_deref(), "status", 128)
            .map_err(serde::de::Error::custom)?;

        let wire_type = wire
            .payload
            .event_type()
            .strip_prefix("response.item.")
            .ok_or_else(|| serde::de::Error::custom("invalid response item payload event type"))?;
        validate_wire_type(wire_type).map_err(serde::de::Error::custom)?;
        if ResponseItemKind::from_wire_type(wire_type) != wire.kind {
            return Err(serde::de::Error::custom(
                "response item kind contradicts payload event type",
            ));
        }
        match wire.payload.payload().as_object() {
            Some(payload) if payload.get("type").is_some() => {
                let payload_type =
                    payload.get("type").and_then(Value::as_str).ok_or_else(|| {
                        serde::de::Error::custom("invalid response item payload type")
                    })?;
                if ResponseItemKind::from_wire_type(payload_type) != wire.kind {
                    return Err(serde::de::Error::custom(
                        "response item kind contradicts payload type",
                    ));
                }
                let payload_id = optional_bounded_string(payload.get("id"), "id", 256)
                    .map_err(serde::de::Error::custom)?;
                let payload_status = optional_bounded_string(payload.get("status"), "status", 128)
                    .map_err(serde::de::Error::custom)?;
                if payload_id != wire.id || payload_status != wire.status {
                    return Err(serde::de::Error::custom(
                        "response item identity contradicts payload",
                    ));
                }
            }
            Some(_) if wire.payload.is_truncated() => {}
            _ => {
                return Err(serde::de::Error::custom(
                    "response item payload must contain its type",
                ));
            }
        }

        let diagnostic = wire
            .kind
            .is_other()
            .then(|| "unknown response item type preserved as bounded Other".to_string());
        if wire.diagnostic != diagnostic {
            return Err(serde::de::Error::custom(
                "response item diagnostic contradicts its kind",
            ));
        }
        Ok(Self {
            id: wire.id,
            status: wire.status,
            kind: wire.kind,
            payload: wire.payload,
            diagnostic,
        })
    }
}

fn validate_wire_type(wire_type: &str) -> Result<(), ResponseItemError> {
    if wire_type.is_empty()
        || wire_type.len() > 114
        || wire_type.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-' | '.')
        })
    {
        return Err(ResponseItemError::InvalidType);
    }
    Ok(())
}

fn validate_bounded_option(
    value: Option<&str>,
    field: &'static str,
    max_bytes: usize,
) -> Result<(), ResponseItemError> {
    match value {
        None => Ok(()),
        Some(value)
            if !value.is_empty()
                && value.len() <= max_bytes
                && !value.chars().any(char::is_control) =>
        {
            Ok(())
        }
        Some(_) => Err(ResponseItemError::InvalidField { field }),
    }
}

fn optional_bounded_string(
    value: Option<&Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, ResponseItemError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if !value.is_empty()
                && value.len() <= max_bytes
                && !value.chars().any(char::is_control) =>
        {
            Ok(Some(value.clone()))
        }
        Some(_) => Err(ResponseItemError::InvalidField { field }),
    }
}

fn require(
    value: &Value,
    field: &'static str,
    predicate: impl FnOnce(&Value) -> bool,
) -> Result<(), ResponseItemError> {
    if value.get(field).is_some_and(predicate) {
        Ok(())
    } else {
        Err(ResponseItemError::InvalidField { field })
    }
}

fn validate_known_payload(kind: &ResponseItemKind, value: &Value) -> Result<(), ResponseItemError> {
    use ResponseItemKind as Kind;
    match kind {
        Kind::Message => {
            require(value, "role", Value::is_string)?;
            require(value, "content", Value::is_array)
        }
        Kind::AgentMessage => {
            require(value, "author", Value::is_string)?;
            require(value, "recipient", Value::is_string)?;
            require(value, "content", Value::is_array)
        }
        Kind::Reasoning => match value.get("summary") {
            None | Some(Value::Null) => Ok(()),
            Some(summary) if summary.is_array() => Ok(()),
            Some(_) => Err(ResponseItemError::InvalidField { field: "summary" }),
        },
        Kind::LocalShellCall => {
            require(value, "status", Value::is_string)?;
            require(value, "action", Value::is_object)
        }
        Kind::FunctionCall => {
            require(value, "name", Value::is_string)?;
            require(value, "arguments", Value::is_string)?;
            require(value, "call_id", Value::is_string)
        }
        Kind::FunctionCallOutput => {
            require(value, "call_id", Value::is_string)?;
            require(value, "output", |_| true)
        }
        Kind::CustomToolCall => {
            require(value, "call_id", Value::is_string)?;
            require(value, "name", Value::is_string)?;
            require(value, "input", Value::is_string)
        }
        Kind::CustomToolCallOutput => {
            require(value, "call_id", Value::is_string)?;
            require(value, "output", |_| true)
        }
        Kind::ToolSearchCall => {
            require(value, "execution", Value::is_string)?;
            require(value, "arguments", |_| true)
        }
        Kind::ToolSearchOutput => {
            require(value, "status", Value::is_string)?;
            require(value, "execution", Value::is_string)?;
            require(value, "tools", Value::is_array)
        }
        Kind::WebSearchCall => Ok(()),
        Kind::ImageGenerationCall => {
            require(value, "status", Value::is_string)?;
            require(value, "result", Value::is_string)
        }
        Kind::Compaction => require(value, "encrypted_content", Value::is_string),
        Kind::CompactionTrigger => Ok(()),
        Kind::ContextCompaction => Ok(()),
        Kind::Other(_) => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResponseItemError {
    #[error("response item must be an object")]
    NotObject,
    #[error("response item type is missing")]
    MissingType,
    #[error("response item type is invalid")]
    InvalidType,
    #[error("response item field {field} is missing or invalid")]
    InvalidField { field: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_baseline_kind_round_trips_with_its_complete_payload() {
        let values = [
            serde_json::json!({"type":"message","id":"m1","role":"assistant","content":[]}),
            serde_json::json!({"type":"agent_message","id":"a1","author":"planner","recipient":"user","content":[]}),
            serde_json::json!({"type":"reasoning","id":"r1","summary":[],"content":[]}),
            serde_json::json!({"type":"local_shell_call","id":"s1","status":"completed","action":{}}),
            serde_json::json!({"type":"function_call","id":"f1","name":"read","arguments":"{}","call_id":"c1"}),
            serde_json::json!({"type":"function_call_output","id":"fo1","call_id":"c1","output":"ok"}),
            serde_json::json!({"type":"custom_tool_call","id":"c1","call_id":"c2","name":"exec","input":"1+1"}),
            serde_json::json!({"type":"custom_tool_call_output","id":"co1","call_id":"c2","output":"2"}),
            serde_json::json!({"type":"tool_search_call","id":"t1","execution":"client","arguments":{}}),
            serde_json::json!({"type":"tool_search_output","id":"to1","status":"completed","execution":"client","tools":[]}),
            serde_json::json!({"type":"web_search_call","id":"w1","status":"completed","action":{"type":"search","query":"q"}}),
            serde_json::json!({"type":"compaction","id":"cp1","encrypted_content":"opaque"}),
            serde_json::json!({"type":"context_compaction","id":"cc1","encrypted_content":"opaque"}),
        ];
        for value in values {
            let item = ResponseItem::from_wire(&value).expect("known item parses");
            assert_eq!(item.payload().payload(), &value);
            let decoded: ResponseItem =
                serde_json::from_value(serde_json::to_value(&item).unwrap()).unwrap();
            assert_eq!(decoded, item);
        }
    }

    #[test]
    fn unknown_and_sensitive_items_are_bounded_and_redacted() {
        let unknown = ResponseItem::from_wire(&serde_json::json!({
            "type": "future_call",
            "credential": "secret",
            "details": "x".repeat(crate::provider::MAX_PROVIDER_EXTENSION_BYTES + 1)
        }))
        .unwrap();
        assert!(unknown.kind().is_other());
        assert!(unknown.diagnostic().is_some());
        assert!(unknown.payload().is_truncated());
        assert!(unknown.payload().was_redacted());

        let image = ResponseItem::from_wire(&serde_json::json!({
            "type":"image_generation_call",
            "status":"completed",
            "result":"base64-binary"
        }))
        .unwrap();
        assert_eq!(image.payload().payload()["result"], "[REDACTED_BINARY]");
        assert!(image.payload().was_redacted());
    }

    #[test]
    fn deserialization_rejects_contradictory_public_fields() {
        let item = ResponseItem::from_wire(&serde_json::json!({
            "type":"web_search_call",
            "id":"ws_1",
            "status":"completed"
        }))
        .unwrap();
        let mut wire = serde_json::to_value(item).unwrap();
        wire["kind"] = serde_json::json!("message");
        assert!(serde_json::from_value::<ResponseItem>(wire).is_err());

        let item = ResponseItem::from_wire(&serde_json::json!({
            "type":"web_search_call",
            "id":"ws_1",
            "status":"completed"
        }))
        .unwrap();
        let mut wire = serde_json::to_value(item).unwrap();
        wire["id"] = serde_json::json!("ws_2");
        assert!(serde_json::from_value::<ResponseItem>(wire).is_err());
    }
}
