//! Extraction of provider response metadata from Responses stream events.

use agent_core::provider::{
    ProviderExtension, ReasoningMetadata, ResponseMetadata, SafetyMetadata,
};
use serde_json::Value;

const MAX_DIAGNOSTIC_ID_BYTES: usize = 256;

pub(crate) fn response_metadata_from_event(event: &Value) -> ResponseMetadata {
    let response = event.get("response");
    let response_headers = response.and_then(|value| value.get("headers"));
    let event_headers = event.get("headers");
    let header = |names: &[&str]| {
        header_value(response_headers, names).or_else(|| header_value(event_headers, names))
    };
    let metadata = event
        .get("metadata")
        .or_else(|| response.and_then(|value| value.get("metadata")));
    let verifications = metadata
        .and_then(|value| value.get("openai_verification_recommendation"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let moderation = metadata
        .and_then(|value| value.get("openai_chatgpt_moderation_metadata"))
        .cloned()
        .map(|value| ProviderExtension::from_value("turn_moderation_metadata", value));
    let safety_value = event
        .get("safety_buffering")
        .or_else(|| response.and_then(|value| value.get("safety_buffering")));
    let strings = |field: &str| {
        safety_value
            .and_then(|value| value.get(field))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    ResponseMetadata {
        response_id: response
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .or_else(|| event.get("response_id").and_then(Value::as_str))
            .map(str::to_string),
        model: response
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| header(&["openai-model", "x-openai-model"])),
        service_tier: response
            .and_then(|value| value.get("service_tier"))
            .and_then(Value::as_str)
            .or_else(|| event.get("service_tier").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| header(&["openai-service-tier"])),
        request_id: request_id_from_event(event),
        turn_state: event
            .get("turn_state")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| header(&["x-codex-turn-state"])),
        models_etag: event
            .get("models_etag")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| header(&["x-models-etag"])),
        end_turn: response
            .and_then(|value| value.get("end_turn"))
            .and_then(Value::as_bool),
        safety: SafetyMetadata {
            use_cases: strings("use_cases"),
            reasons: strings("reasons"),
            retry_model: safety_value
                .and_then(|value| value.get("retry_model"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        verifications,
        moderation,
        reasoning: ReasoningMetadata {
            server_included: event
                .get("reasoning_included")
                .and_then(Value::as_bool)
                .or_else(|| header(&["x-reasoning-included"]).map(|_| true)),
            ..ReasoningMetadata::default()
        },
    }
}

pub(crate) fn request_id_from_event(event: &Value) -> Option<String> {
    diagnostic_id_from_event(event, &["request_id"], &["x-request-id", "request-id"])
}

pub(crate) fn auth_request_id_from_event(event: &Value) -> Option<String> {
    diagnostic_id_from_event(
        event,
        &["auth_request_id"],
        &["x-auth-request-id", "auth-request-id"],
    )
}

pub(crate) fn bounded_diagnostic_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_DIAGNOSTIC_ID_BYTES
        && !value.chars().any(char::is_control))
    .then(|| value.to_string())
}

fn diagnostic_id_from_event(event: &Value, fields: &[&str], headers: &[&str]) -> Option<String> {
    direct_string(event, fields)
        .or_else(|| {
            event
                .get("response")
                .and_then(|value| direct_string(value, fields))
        })
        .or_else(|| {
            let response_headers = event.get("response").and_then(|value| value.get("headers"));
            header_value(response_headers, headers)
        })
        .or_else(|| header_value(event.get("headers"), headers))
        .as_deref()
        .and_then(bounded_diagnostic_id)
}

fn direct_string(value: &Value, names: &[&str]) -> Option<String> {
    value.as_object()?.iter().find_map(|(name, value)| {
        names
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            .then(|| value.as_str().map(str::to_string))
            .flatten()
    })
}

fn header_value(headers: Option<&Value>, names: &[&str]) -> Option<String> {
    headers?.as_object()?.iter().find_map(|(name, value)| {
        names
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            .then(|| match value {
                Value::String(value) => Some(value.clone()),
                Value::Array(values) => values.first().and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .flatten()
    })
}
