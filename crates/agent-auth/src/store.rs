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
    #[error("keyring entry `{account}` holds a credential of another kind")]
    UnexpectedCredential { account: String },
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
    let blob = match entry.get_secret() {
        Ok(blob) => blob,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match serde_json::from_slice(&blob) {
        Ok(cred) => Ok(Some(cred)),
        Err(error) => read_legacy_entry(&entry, account, error).map(Some),
    }
}

/// Reads an entry written before this crate moved from `set_password` to
/// `set_secret`. Windows stores a password as UTF-16, so `get_secret` returns
/// bytes that are not the JSON we wrote; `get_password` decodes them.
///
/// The entry is rewritten through the current path, so this runs once per
/// stale entry rather than on every read forever. `secret_error` is the failure
/// of the normal path and stays the one reported: a caller debugging a corrupt
/// entry wants to hear about the read that was supposed to work.
fn read_legacy_entry(
    entry: &keyring::Entry,
    account: &str,
    secret_error: serde_json::Error,
) -> Result<Credential, StoreError> {
    let Ok(legacy) = entry.get_password() else {
        return Err(secret_error.into());
    };
    let Ok(cred) = serde_json::from_str::<Credential>(&legacy) else {
        return Err(secret_error.into());
    };
    if let Err(error) = save(account, &cred) {
        // Not fatal: the credential was read and is usable. The migration just
        // gets another chance on the next read.
        tracing::warn!(
            target: "pyxis::auth",
            account,
            error = %error,
            "legacy keyring entry could not be rewritten through set_secret"
        );
    }
    Ok(cred)
}

/// Deletes a credential (idempotent: absent == success).
pub fn delete(account: &str) -> Result<(), StoreError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
