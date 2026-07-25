//! Storage of the credentials in the OS secret store (US-018): Secret
//! Service / keyring, never in clear text on disk. We do NOT replicate Pi's
//! clear-text `~/.pi/agent/auth.json`: the JSON blob (tokens included) lives in
//! the keyring, encrypted by the OS.
//!
//! We use `set_secret` rather than `set_password`: on Windows, `keyring`
//! encodes passwords in UTF-16 before `CredWriteW`, which needlessly halves
//! the space available for the OAuth tokens.

use crate::Credential;

const SERVICE: &str = "pyxis";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("secret store unavailable: {0} (fallback: env var, see docs)")]
    Keyring(#[from] keyring::Error),
    #[error("credential serialization/deserialization: {0}")]
    Serde(#[from] serde_json::Error),
}

fn entry(account: &str) -> Result<keyring::Entry, StoreError> {
    Ok(keyring::Entry::new(SERVICE, account)?)
}

/// Persists a credential (JSON blob) into the keyring under the `account` key
/// (typically `oauth:openai_chatgpt` or `apikey:openai_chat`).
pub fn save(account: &str, cred: &Credential) -> Result<(), StoreError> {
    let blob = serde_json::to_vec(cred)?;
    entry(account)?.set_secret(&blob)?;
    Ok(())
}

/// Reads a credential, `None` when absent.
pub fn load(account: &str) -> Result<Option<Credential>, StoreError> {
    let entry = entry(account)?;
    match entry.get_secret() {
        Ok(blob) => match serde_json::from_slice(&blob) {
            Ok(cred) => Ok(Some(cred)),
            Err(secret_err) => match entry.get_password() {
                Ok(password_blob) => Ok(Some(serde_json::from_str(&password_blob)?)),
                Err(keyring::Error::NoEntry) => Err(secret_err.into()),
                Err(e) => Err(e.into()),
            },
        },
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Deletes a credential (idempotent: absent == success).
pub fn delete(account: &str) -> Result<(), StoreError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
