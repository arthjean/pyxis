use reqwest::header::{HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use crate::Secret;

use super::ProviderAuthError;

pub(super) fn bearer_headers(token: &Secret) -> Result<Vec<(String, String)>, ProviderAuthError> {
    let value = validated_header_value("token", &format!("Bearer {}", token.expose()))?;
    Ok(vec![("authorization".into(), value)])
}

pub(super) fn validate_prebuilt_headers(
    headers: &[(String, Secret)],
) -> Result<Vec<(String, String)>, ProviderAuthError> {
    if headers.is_empty() {
        return Err(invalid("headers", "must contain at least one header"));
    }
    headers
        .iter()
        .map(|(name, value)| {
            let normalized = name.to_ascii_lowercase();
            if matches!(
                normalized.as_str(),
                "host" | "content-length" | "content-encoding" | "transfer-encoding" | "connection"
            ) {
                return Err(invalid("headers", format!("header `{name}` is forbidden")));
            }
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("headers", format!("header name `{name}` is invalid")))?;
            Ok((
                normalized,
                validated_header_value("headers", value.expose())?,
            ))
        })
        .collect()
}

pub(super) fn validated_header_value(
    field: &'static str,
    value: &str,
) -> Result<String, ProviderAuthError> {
    HeaderValue::from_str(value)
        .map_err(|_| invalid(field, "contains an invalid HTTP header value"))?;
    Ok(value.to_owned())
}

pub(super) fn validate_identity(field: &'static str, value: &str) -> Result<(), ProviderAuthError> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid(field, "must be a non-empty printable value"));
    }
    Ok(())
}

pub(super) fn add_fedramp_header(headers: &mut Vec<(String, String)>, fedramp: bool) {
    if fedramp {
        headers.push(("x-openai-fedramp".into(), "true".into()));
    }
}

pub(super) fn fingerprint(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(super) fn invalid(field: &'static str, reason: impl Into<String>) -> ProviderAuthError {
    ProviderAuthError::InvalidField {
        field,
        reason: reason.into(),
    }
}
