//! Extraction of provider response metadata from Responses stream events.

use agent_core::provider::{
    ProviderExtension, ReasoningMetadata, ResponseMetadata, SafetyMetadata,
};
use serde_json::Value;

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
        request_id: event
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| header(&["x-request-id", "request-id"])),
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
