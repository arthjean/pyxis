//! The code -> token exchange, the rotating refresh, and the JWT claim the
//! ChatGPT backend routes on.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use super::{CLIENT_ID, JWT_CLAIM_NAMESPACE, TOKEN_URL};
use crate::oauth::AuthError;
use crate::{OAuthCredential, ProviderId, Secret};

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

/// Decodes (without verifying the signature) the payload of a JWT and extracts
/// `chatgpt_account_id` from it. We do not verify the signature: we read a claim, and
/// trust comes from OpenAI's TLS channel, not from a local crypto validation.
pub fn extract_account_id(access_token: &str) -> Result<String, AuthError> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get(JWT_CLAIM_NAMESPACE)
        .and_then(|ns| ns.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or(AuthError::MissingAccountId)
}

fn decode_jwt_payload(jwt: &str) -> Result<serde_json::Value, AuthError> {
    let payload_b64 = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| AuthError::Jwt("payload missing".to_string()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| AuthError::Jwt(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| AuthError::Jwt(e.to_string()))
}

pub(super) fn token_to_credential(
    token: TokenResponse,
    now_ms: u64,
) -> Result<OAuthCredential, AuthError> {
    let account_id = extract_account_id(&token.access_token)?;
    Ok(OAuthCredential {
        provider: ProviderId::OpenAiChatGpt,
        access: Secret::new(token.access_token),
        refresh: Secret::new(token.refresh_token),
        // sliding: absolute expiry = now + expires_in (seconds -> ms)
        expires_at: now_ms.saturating_add(token.expires_in.saturating_mul(1000)),
        account_id: Some(account_id),
    })
}

/// Exchanges an `authorization_code` for tokens. `redirect_uri` differs
/// between browser (`REDIRECT_URI`) and device (`DEVICE_REDIRECT_URI`).
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &Secret,
    verifier: &Secret,
    redirect_uri: &str,
    now_ms: u64,
) -> Result<OAuthCredential, AuthError> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code.expose()),
            ("code_verifier", verifier.expose()),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = resp.json().await?;
    token_to_credential(token, now_ms)
}

/// Refreshes a credential through `grant_type=refresh_token`. The refresh is
/// **rotating**: the new credential carries a new refresh token to rewrite.
pub async fn refresh(
    client: &reqwest::Client,
    refresh_token: &str,
    now_ms: u64,
) -> Result<OAuthCredential, AuthError> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = resp.json().await?;
    token_to_credential(token, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.sig")
    }

    fn account_jwt(account: &str) -> String {
        make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account }
        }))
    }

    #[test]
    fn extract_account_id_reads_custom_claim() {
        assert_eq!(
            extract_account_id(&account_jwt("acct_42")).unwrap(),
            "acct_42"
        );
    }

    #[test]
    fn extract_account_id_missing_claim_errors() {
        let jwt = make_jwt(&serde_json::json!({ "sub": "user_1" }));
        assert!(matches!(
            extract_account_id(&jwt),
            Err(AuthError::MissingAccountId)
        ));
    }

    #[test]
    fn token_to_credential_sets_provider_and_sliding_expiry() {
        let token = TokenResponse {
            access_token: account_jwt("acct_9"),
            refresh_token: "rt".to_string(),
            expires_in: 3600,
        };
        let cred = token_to_credential(token, 1_000).unwrap();
        assert_eq!(cred.provider, ProviderId::OpenAiChatGpt);
        assert_eq!(cred.account_id.as_deref(), Some("acct_9"));
        assert_eq!(cred.expires_at, 1_000 + 3_600_000);
        assert!(!cred.needs_refresh(cred.expires_at - 1, 0));
        assert!(cred.needs_refresh(cred.expires_at, 0));
    }
}
