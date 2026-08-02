//! Safe representation of provider events that have no canonical variant yet.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::redaction::{redact_json_value, redact_string};

/// Maximum serialized provider payload allowed across the public event seam.
pub const MAX_PROVIDER_EXTENSION_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_EXTENSION_TYPE_CHARS: usize = 128;

/// A bounded and sanitized provider event.
///
/// Fields stay private so callers cannot bypass [`ProviderExtension::from_value`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderExtension {
    event_type: String,
    payload: Value,
    original_bytes: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    redacted: bool,
}

impl ProviderExtension {
    pub fn from_value(event_type: impl Into<String>, payload: Value) -> Self {
        Self::from_value_with_redaction(event_type, payload, false)
    }

    /// Builds an extension after a caller has already removed a payload that is
    /// sensitive by contract rather than by field name (for example generated
    /// image bytes). The flag remains observable after the value is replaced.
    pub fn from_value_with_redaction(
        event_type: impl Into<String>,
        payload: Value,
        already_redacted: bool,
    ) -> Self {
        Self::build(event_type, payload, already_redacted, None)
    }

    pub fn from_redacted_value(
        event_type: impl Into<String>,
        payload: Value,
        original_bytes: u64,
    ) -> Self {
        Self::build(event_type, payload, true, Some(original_bytes))
    }

    fn build(
        event_type: impl Into<String>,
        payload: Value,
        already_redacted: bool,
        original_bytes_override: Option<u64>,
    ) -> Self {
        let event_type: String = event_type
            .into()
            .chars()
            .take(MAX_PROVIDER_EXTENSION_TYPE_CHARS)
            .collect();
        let (event_type, type_redacted) = redact_string(event_type);
        let original_bytes = original_bytes_override.unwrap_or_else(|| {
            serde_json::to_vec(&payload)
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(u64::MAX)
        });
        let (payload, payload_redacted) = redact_json_value(payload);
        let redacted = already_redacted || type_redacted || payload_redacted;
        if original_bytes > MAX_PROVIDER_EXTENSION_BYTES as u64 {
            return Self {
                event_type,
                payload: serde_json::json!({
                    "diagnostic": "provider payload omitted after redaction because it exceeds the configured bound",
                    "original_bytes": original_bytes,
                }),
                original_bytes,
                truncated: true,
                redacted,
            };
        }
        Self {
            event_type,
            payload,
            original_bytes,
            truncated: false,
            redacted,
        }
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    pub fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn original_bytes(&self) -> u64 {
        self.original_bytes
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn was_redacted(&self) -> bool {
        self.redacted
    }
}

#[derive(Deserialize)]
struct ProviderExtensionWire {
    event_type: String,
    payload: Value,
    #[serde(default)]
    original_bytes: u64,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    redacted: bool,
}

impl<'de> Deserialize<'de> for ProviderExtension {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProviderExtensionWire::deserialize(deserializer)?;
        let mut extension = Self::from_value(wire.event_type, wire.payload);
        extension.original_bytes = extension.original_bytes.max(wire.original_bytes);
        extension.truncated |= wire.truncated;
        extension.redacted |= wire.redacted;
        Ok(extension)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_redact_common_secrets_and_bound_the_original_payload() {
        let sensitive = ProviderExtension::from_value(
            "response.future",
            serde_json::json!({
                "session_token": "ST",
                "password": "PW",
                "asset": "https://uploads.invalid/a?X-Amz-Signature=secret",
                "cause": "future item"
            }),
        );
        assert_eq!(sensitive.event_type(), "response.future");
        assert!(sensitive.was_redacted());
        assert_eq!(sensitive.payload()["session_token"], "[REDACTED]");
        assert_eq!(sensitive.payload()["password"], "[REDACTED]");
        assert_eq!(sensitive.payload()["asset"], "[REDACTED_SIGNED_URL]");
        assert_eq!(sensitive.payload()["cause"], "future item");

        let oversized = ProviderExtension::from_value(
            "response.huge",
            serde_json::json!({"access_token": "x".repeat(MAX_PROVIDER_EXTENSION_BYTES + 1)}),
        );
        assert!(oversized.is_truncated());
        assert!(oversized.original_bytes() > MAX_PROVIDER_EXTENSION_BYTES as u64);
        assert!(serde_json::to_vec(&oversized).unwrap().len() < MAX_PROVIDER_EXTENSION_BYTES);
    }

    #[test]
    fn deserialization_reapplies_the_invariant_instead_of_trusting_flags() {
        let decoded: ProviderExtension = serde_json::from_value(serde_json::json!({
            "event_type": "response.future",
            "payload": {"id_token": "secret"},
            "original_bytes": 1,
            "truncated": false,
            "redacted": false
        }))
        .unwrap();
        assert_eq!(decoded.payload()["id_token"], "[REDACTED]");
        assert!(decoded.was_redacted());
    }
}
