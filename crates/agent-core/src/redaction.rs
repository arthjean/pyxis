//! Canonical scrubbing for provider-controlled diagnostics.
//!
//! Provider adapters can receive arbitrary JSON and error text. This module is
//! the single policy for deciding what may cross the public event boundary.

use serde_json::Value;

const REDACTED: &str = "[REDACTED]";
const REDACTED_SIGNED_URL: &str = "[REDACTED_SIGNED_URL]";

/// Whether a JSON field name conventionally carries a credential, secret or
/// account identifier. Token counters such as `input_tokens` are deliberately
/// not classified as credentials.
pub fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    key == "authorization"
        || key.contains("credential")
        || key.contains("password")
        || key.contains("secret")
        || key == "token"
        || key.ends_with("_token")
        || key.contains("access_token")
        || key.contains("refresh_token")
        || key.contains("id_token")
        || key.contains("session_token")
        || key == "api_key"
        || key == "apikey"
        || key.ends_with("_api_key")
        || key == "cookie"
        || key.ends_with("_cookie")
        || key.contains("account_id")
}

/// Whether a string contains an HTTP URL carrying a signed query parameter.
/// Signed upload URLs are credentials even when their host is public.
pub fn looks_like_signed_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let Some(start) = lower.find("https://").or_else(|| lower.find("http://")) else {
        return false;
    };
    let url = &lower[start..];
    [
        "x-amz-signature",
        "x-amz-credential",
        "x-goog-signature",
        "x-goog-credential",
        "signature",
        "sig",
        "token",
        "se",
    ]
    .iter()
    .any(|parameter| {
        url.contains(&format!("?{parameter}=")) || url.contains(&format!("&{parameter}="))
    })
}

/// Scrubs an arbitrary provider string. The whole value is replaced when it
/// contains credential syntax because preserving fragments risks retaining a
/// token under an unknown wire format.
pub fn redact_string(value: String) -> (String, bool) {
    if looks_like_signed_url(&value) {
        return (REDACTED_SIGNED_URL.to_string(), true);
    }
    let lower = value.to_ascii_lowercase();
    let trimmed = value.trim();
    let credential_marker = [
        "bearer ",
        "authorization:",
        "access_token=",
        "refresh_token=",
        "id_token=",
        "session_token=",
        "api_key=",
        "apikey=",
        "account_id=",
        "chatgpt-account-id=",
        "credential=",
        "password=",
        "secret=",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let token_shape = trimmed.starts_with("sk-")
        || (trimmed.starts_with("eyJ") && trimmed.matches('.').count() >= 2);
    if credential_marker || token_shape {
        return (REDACTED.to_string(), true);
    }
    (value, false)
}

/// Recursively sanitizes provider-controlled JSON and reports whether anything
/// was replaced.
pub fn redact_json_value(value: Value) -> (Value, bool) {
    match value {
        Value::Object(entries) => {
            let mut redacted = false;
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    if is_sensitive_key(&key) {
                        redacted = true;
                        return (key, Value::String(REDACTED.to_string()));
                    }
                    let (value, child_redacted) = redact_json_value(value);
                    redacted |= child_redacted;
                    (key, value)
                })
                .collect();
            (Value::Object(entries), redacted)
        }
        Value::Array(items) => {
            let mut redacted = false;
            let items = items
                .into_iter()
                .map(|item| {
                    let (item, child_redacted) = redact_json_value(item);
                    redacted |= child_redacted;
                    item
                })
                .collect();
            (Value::Array(items), redacted)
        }
        Value::String(text) => {
            let (text, redacted) = redact_string(text);
            (Value::String(text), redacted)
        }
        other => (other, false),
    }
}

/// Sanitizes a non-JSON provider error while preserving its non-sensitive
/// context. JSON callers should use [`redact_json_value`] first.
pub fn redact_text(input: &str) -> String {
    if looks_like_signed_url(input) {
        return REDACTED_SIGNED_URL.to_string();
    }
    let mut text = redact_bearer_tokens(input);
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "session_token",
        "authorization",
        "chatgpt-account-id",
        "chatgpt_account_id",
        "account_id",
        "api_key",
        "apikey",
        "x-api-key",
        "cookie",
        "set-cookie",
        "credential",
        "password",
        "secret",
    ] {
        text = redact_assignment(&text, key);
    }
    text
}

fn redact_bearer_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.to_ascii_lowercase().find("bearer ") {
        let (before, with_marker) = rest.split_at(position);
        out.push_str(before);
        out.push_str("Bearer [REDACTED]");
        let value = &with_marker["bearer ".len()..];
        let end = value
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']')
            })
            .unwrap_or(value.len());
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

fn redact_assignment(input: &str, key: &str) -> String {
    let marker = format!("{key}=");
    let lower_marker = marker.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.to_ascii_lowercase().find(&lower_marker) {
        let (before, with_marker) = rest.split_at(position);
        out.push_str(before);
        out.push_str(&with_marker[..marker.len()]);
        out.push_str(REDACTED);
        let value = &with_marker[marker.len()..];
        let end = value
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '&' | '"' | '\'' | ',' | ';')
            })
            .unwrap_or(value.len());
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_cover_credentials_without_eating_token_counters() {
        for key in [
            "session_token",
            "id-token",
            "password",
            "secret",
            "apikey",
            "x-api-key",
            "set-cookie",
            "chatgpt_account_id",
        ] {
            assert!(is_sensitive_key(key), "{key}");
        }
        assert!(!is_sensitive_key("input_tokens"));
        assert!(!is_sensitive_key("max_tokens"));
    }

    #[test]
    fn raw_error_text_keeps_context_but_removes_credentials() {
        let sanitized =
            redact_text("bad Authorization: Bearer AT session_token=ST&chatgpt-account-id=acct_1");
        assert!(sanitized.contains("bad Authorization:"));
        assert!(!sanitized.contains(" AT"));
        assert!(!sanitized.contains("ST"));
        assert!(!sanitized.contains("acct_1"));

        let signed =
            redact_text("upload failed at https://uploads.invalid/a?part=1&X-Amz-Signature=secret");
        assert_eq!(signed, REDACTED_SIGNED_URL);
    }
}
