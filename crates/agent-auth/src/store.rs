//! Stockage des credentials dans le secret store de l'OS (US-018) — Secret
//! Service / keyring, jamais en clair sur disque. On NE réplique PAS le
//! `~/.pi/agent/auth.json` clair de Pi : le blob JSON (tokens inclus) vit dans
//! le keyring, chiffré par l'OS.

use crate::Credential;

const SERVICE: &str = "numen";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("secret store indisponible : {0} (fallback : variable d'env, cf. doc)")]
    Keyring(#[from] keyring::Error),
    #[error("(dé)sérialisation credential : {0}")]
    Serde(#[from] serde_json::Error),
}

fn entry(account: &str) -> Result<keyring::Entry, StoreError> {
    Ok(keyring::Entry::new(SERVICE, account)?)
}

/// Persiste une credential (blob JSON) dans le keyring sous la clé `account`
/// (typiquement `oauth:openai_chatgpt` ou `apikey:openai_chat`).
pub fn save(account: &str, cred: &Credential) -> Result<(), StoreError> {
    let blob = serde_json::to_string(cred)?;
    entry(account)?.set_password(&blob)?;
    Ok(())
}

/// Lit une credential, `None` si absente.
pub fn load(account: &str) -> Result<Option<Credential>, StoreError> {
    match entry(account)?.get_password() {
        Ok(blob) => Ok(Some(serde_json::from_str(&blob)?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Supprime une credential (idempotent : absente == succès).
pub fn delete(account: &str) -> Result<(), StoreError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
