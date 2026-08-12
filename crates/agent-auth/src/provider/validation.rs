use reqwest::header::HeaderValue;
use sha2::{Digest, Sha256};

use crate::Secret;

use super::ProviderAuthError;

pub(super) fn bearer_headers(token: &Secret) -> Result<Vec<(String, Secret)>, ProviderAuthError> {
    let value = validated_header_value("token", &format!("Bearer {}", token.expose()))?;
    Ok(vec![("authorization".into(), value)])
}

pub(super) fn validated_header_value(
    field: &'static str,
    value: &str,
) -> Result<Secret, ProviderAuthError> {
    HeaderValue::from_str(value)
        .map_err(|_| invalid(field, "contains an invalid HTTP header value"))?;
    Ok(Secret::new(value))
}

/// An identity ends up in logs and in a fingerprint, so it must be something
/// that can be printed on one line.
pub(super) fn validate_identity(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), ProviderAuthError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid(field, "must be a non-empty printable value"));
    }
    Ok(())
}

/// What a fingerprint is computed over. The fallback used to be an inline
/// `identity.unwrap_or_else(|| key.expose())`, which made "an account named X"
/// and "an unnamed account whose key is X" indistinguishable once hashed.
pub(super) enum IdentitySource<'a> {
    /// The caller named the account. Rotating the secret keeps the identity.
    Named(&'a str),
    /// No name available: the secret stands in for one, so a rotation reads as
    /// a different identity and re-scopes whatever depends on it.
    DerivedFromSecret(&'a Secret),
    /// Nothing to identify: no credential was presented.
    Anonymous,
}

impl<'a> IdentitySource<'a> {
    pub(super) fn of(identity: Option<&'a str>, secret: &'a Secret) -> Self {
        match identity {
            Some(identity) => Self::Named(identity),
            None => Self::DerivedFromSecret(secret),
        }
    }

    fn tagged(&self) -> (&'static str, &str) {
        match self {
            Self::Named(identity) => ("named", identity),
            Self::DerivedFromSecret(secret) => ("secret", secret.expose()),
            Self::Anonymous => ("anonymous", ""),
        }
    }
}

/// A stable, non-secret handle on "which account is this".
pub(super) fn identity_fingerprint(scope: &[&str], identity: IdentitySource<'_>) -> String {
    let (tag, value) = identity.tagged();
    let mut hash = Sha256::new();
    for part in scope.iter().copied().chain([tag, value]) {
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
