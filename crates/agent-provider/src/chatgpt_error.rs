//! Shared ChatGPT API error decoding for HTTP responses and streamed events.

use agent_core::provider::{ProviderError, ProviderErrorCategory};
use serde_json::Value;

use crate::chatgpt_metadata::{
    auth_request_id_from_event, bounded_diagnostic_id, request_id_from_event,
};

const MAX_ERROR_MESSAGE_CHARS: usize = 2_000;

const TERMINAL_RATE_LIMIT_MARKERS: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
];

pub(crate) async fn from_http_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    from_http_parts(status, &headers, &body)
}

/// Decodes an HTTP failure independently of the transport that performed the
/// handshake. HTTP/SSE and WebSocket upgrade failures must expose the same
/// category, retry delay, and bounded diagnostic identifiers.
pub(crate) fn from_http_parts(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> ProviderError {
    let request_id = diagnostic_header(headers, &["x-request-id", "request-id"]);
    let auth_request_id = diagnostic_header(headers, &["x-auth-request-id", "auth-request-id"]);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let retry_after_ms = parse_retry_after_ms(headers, now_ms);
    let quota = crate::quota::parse_quota_headers(headers);
    let mut message = bounded_error_body(body);
    let category = api_category_for_http(status, &message);
    if category == ProviderErrorCategory::Quota {
        message = format!(
            "{} {message}",
            crate::quota::quota_refusal_message(quota.as_ref())
        );
    }

    tracing::warn!(
        target: "pyxis::provider",
        status,
        retry_after_ms,
        "provider request failed"
    );
    ProviderError::Api {
        category,
        status: Some(status),
        message,
        retry_after_ms,
        request_id,
        auth_request_id,
    }
}

pub(crate) fn stream_error(event: &Value) -> ProviderError {
    let error = event.get("error").unwrap_or(event);
    classified_stream_error(error, event)
}

pub(crate) fn failed_error(event: &Value) -> ProviderError {
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .unwrap_or(event);
    classified_stream_error(error, event)
}

pub(crate) fn incomplete_error(event: &Value) -> ProviderError {
    let reason = event
        .get("response")
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    api_error(
        ProviderErrorCategory::Incomplete,
        Some(400),
        &format!("response incomplete: {reason}"),
        None,
        event,
    )
}

pub(crate) fn terminal_error(
    category: ProviderErrorCategory,
    status: Option<u16>,
    message: &str,
    event: &Value,
) -> ProviderError {
    api_error(category, status, message, None, event)
}

pub(crate) fn invalid_request(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::Api {
        category: ProviderErrorCategory::InvalidRequest,
        status: None,
        message: truncate_chars(&error.to_string(), MAX_ERROR_MESSAGE_CHARS),
        retry_after_ms: None,
        request_id: None,
        auth_request_id: None,
    }
}

fn classified_stream_error(error: &Value, envelope: &Value) -> ProviderError {
    let code = error.get("code").and_then(Value::as_str);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("provider response failed");
    let category = classify_api_category(None, code, message);
    let status = inferred_status(category);
    let detail = code.map_or_else(|| message.to_string(), |code| format!("{code}: {message}"));
    api_error(
        category,
        status,
        &detail,
        retry_delay_from_error(error, message),
        envelope,
    )
}

pub(crate) fn classify_api_category(
    status: Option<u16>,
    code: Option<&str>,
    message: &str,
) -> ProviderErrorCategory {
    let code = code.unwrap_or_default().to_ascii_lowercase();
    let haystack = format!("{code} {message}").to_ascii_lowercase();
    match status {
        Some(413) => return ProviderErrorCategory::ContextOverflow,
        Some(401) => return ProviderErrorCategory::Authentication,
        Some(403) => return ProviderErrorCategory::PermissionDenied,
        Some(429) if is_terminal_rate_limit(&haystack) => return ProviderErrorCategory::Quota,
        Some(429) => return ProviderErrorCategory::RateLimited,
        Some(529) => return ProviderErrorCategory::Overloaded,
        _ => {}
    }
    if code == "context_length_exceeded"
        || (haystack.contains("context")
            && (haystack.contains("length") || haystack.contains("too long")))
    {
        ProviderErrorCategory::ContextOverflow
    } else if code == "insufficient_quota" || is_terminal_rate_limit(&haystack) {
        ProviderErrorCategory::Quota
    } else if code == "usage_not_included" || haystack.contains("usage not included") {
        ProviderErrorCategory::UsageNotIncluded
    } else if code.contains("cyber") || haystack.contains("cyber policy") {
        ProviderErrorCategory::CyberPolicy
    } else if code == "invalid_prompt" || code == "bio_policy" {
        ProviderErrorCategory::InvalidPrompt
    } else if code.contains("image") || haystack.contains("invalid image") {
        ProviderErrorCategory::InvalidImage
    } else if code == "rate_limit_exceeded" || haystack.contains("too many requests") {
        ProviderErrorCategory::RateLimited
    } else if code == "server_is_overloaded" || code == "slow_down" || haystack.contains("overload")
    {
        ProviderErrorCategory::Overloaded
    } else if code.contains("auth")
        || haystack.contains("unauthorized")
        || haystack.contains("invalid token")
        || haystack.contains("expired token")
    {
        ProviderErrorCategory::Authentication
    } else if code.contains("permission") || haystack.contains("forbidden") {
        ProviderErrorCategory::PermissionDenied
    } else if code == "invalid_request_error" || code == "invalid_request" {
        ProviderErrorCategory::InvalidRequest
    } else {
        ProviderErrorCategory::Failed
    }
}

pub(crate) fn api_category_for_http(status: u16, body: &str) -> ProviderErrorCategory {
    let code = error_code_from_body(body);
    classify_api_category(Some(status), code.as_deref(), body)
}

fn inferred_status(category: ProviderErrorCategory) -> Option<u16> {
    Some(match category {
        ProviderErrorCategory::ContextOverflow => 413,
        ProviderErrorCategory::Quota | ProviderErrorCategory::RateLimited => 429,
        ProviderErrorCategory::Overloaded => 529,
        ProviderErrorCategory::Authentication => 401,
        ProviderErrorCategory::PermissionDenied => 403,
        ProviderErrorCategory::Failed => 503,
        ProviderErrorCategory::UsageNotIncluded
        | ProviderErrorCategory::CyberPolicy
        | ProviderErrorCategory::InvalidPrompt
        | ProviderErrorCategory::InvalidImage
        | ProviderErrorCategory::Incomplete
        | ProviderErrorCategory::InvalidRequest => 400,
    })
}

fn api_error(
    category: ProviderErrorCategory,
    status: Option<u16>,
    message: &str,
    retry_after_ms: Option<u64>,
    envelope: &Value,
) -> ProviderError {
    ProviderError::Api {
        category,
        status,
        message: truncate_chars(
            &agent_core::redaction::redact_text(message),
            MAX_ERROR_MESSAGE_CHARS,
        ),
        retry_after_ms,
        request_id: request_id_from_event(envelope),
        auth_request_id: auth_request_id_from_event(envelope),
    }
}

fn diagnostic_header(headers: &reqwest::header::HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(bounded_diagnostic_id)
    })
}

fn error_code_from_body(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn retry_delay_from_error(error: &Value, message: &str) -> Option<u64> {
    if let Some(milliseconds) = error.get("retry_after_ms").and_then(Value::as_u64) {
        return Some(milliseconds);
    }
    if let Some(seconds) = error.get("retry_after").and_then(Value::as_f64)
        && seconds.is_finite()
    {
        return Some((seconds.max(0.0) * 1_000.0) as u64);
    }
    let rest = message
        .to_ascii_lowercase()
        .split_once("try again in ")?
        .1
        .to_string();
    let numeric_len = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .count();
    let number = rest.get(..numeric_len)?.parse::<f64>().ok()?;
    if !number.is_finite() {
        return None;
    }
    let suffix = rest.get(numeric_len..)?.trim_start();
    Some(if suffix.starts_with("ms") {
        number.max(0.0) as u64
    } else {
        (number.max(0.0) * 1_000.0) as u64
    })
}

pub(crate) fn should_retry_without_reasoning_replay(status: u16, message: &str) -> bool {
    if status != 400 {
        return false;
    }
    let message = message.to_ascii_lowercase();
    message.contains("encrypted_reasoning")
        || message.contains("encrypted reasoning")
        || message.contains("reasoning replay")
}

pub(crate) fn is_terminal_rate_limit(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    TERMINAL_RATE_LIMIT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

pub(crate) fn sanitize_error_body(body: &str) -> String {
    let text = if let Ok(value) = serde_json::from_str::<Value>(body) {
        let (value, _) = agent_core::redaction::redact_json_value(value);
        serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
    } else {
        body.to_string()
    };
    agent_core::redaction::redact_text(&text)
}

pub(crate) fn bounded_error_body(body: &str) -> String {
    truncate_chars(&sanitize_error_body(body), MAX_ERROR_MESSAGE_CHARS)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(crate) fn parse_retry_after_ms(
    headers: &reqwest::header::HeaderMap,
    now_ms: u64,
) -> Option<u64> {
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|milliseconds| milliseconds.is_finite())
    {
        return Some(milliseconds.max(0.0) as u64);
    }
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Some(seconds) = raw
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite())
    {
        return Some((seconds.max(0.0) * 1_000.0) as u64);
    }
    let target_ms = parse_imf_fixdate_ms(raw)?;
    Some(target_ms.saturating_sub(now_ms))
}

pub(crate) fn parse_imf_fixdate_ms(value: &str) -> Option<u64> {
    let rest = value.trim().split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month: i64 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut time = parts.next()?.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    if seconds < 0 {
        return None;
    }
    Some((seconds as u64) * 1_000)
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
