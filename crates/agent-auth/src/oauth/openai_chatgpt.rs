//! ChatGPT subscription auth (ADR-10). Reuses the OAuth client of the **official
//! OSS Codex CLI**: PKCE S256 flow on `auth.openai.com` (browser + device-code),
//! JWT decoding for `chatgpt_account_id`, rotating refresh tokens.
//!
//! Constants checked verbatim against the Pi repo (`packages/ai/src/utils/oauth/
//! openai-codex.ts` + `providers/openai-codex-responses.ts`, 45/45 confirmed).
//! Details & sources: `docs/openai-subscription-auth.md`.
//!
//! ToS grey area: it impersonates Codex (shared client_id), **revocable
//! unilaterally by OpenAI** (see ADR-7 R1, ADR-10). A "fragile" credential,
//! never on the critical path: it is a dogfooding convenience behind BYOK.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::Deserialize;

use super::pkce::Pkce;
use crate::provider::ProviderRequestAuth;
use crate::{OAuthCredential, ProviderId, Secret};

// ───────────────── Auth constants (auth.openai.com), verbatim from Pi ─────────────────

/// `client_id` of the OSS Codex CLI (`openai-codex.ts:31`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// URI displayed to the user in device flow.
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
/// `redirect_uri` of the code -> token exchange in **device** flow (different from browser).
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(900);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(900);
/// Namespace of the custom claim where `chatgpt_account_id` lives (`openai-codex.ts:44`).
pub const JWT_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";
/// Hardcoded per client (Pi uses `"pi"`). The ChatGPT backend **may** validate
/// the `originator` against a known list: to be tested on the first run (ADR-10).
pub const ORIGINATOR: &str = "pyxis";

/// `originator` fallback when the backend rejects `pyxis` (US-021, unhappy path):
/// borrow the identity of the official OSS Codex CLI, already on the backend
/// allow-list. Switched at runtime through `PYXIS_ORIGINATOR` (no recompilation).
pub const ORIGINATOR_FALLBACK: &str = "codex_cli_rs";

/// Effective `originator` sent on the INFERENCE request (US-021). Reads
/// `PYXIS_ORIGINATOR` (allows switching `pyxis` <-> `codex_cli_rs` during the
/// spike without recompiling); default `ORIGINATOR`. Does NOT affect the OAuth flow:
/// `build_authorize_url` keeps `ORIGINATOR` (changing the auth would break the flow
/// validated live, out of scope).
pub fn originator() -> String {
    match std::env::var("PYXIS_ORIGINATOR") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => ORIGINATOR.to_string(),
    }
}

/// Deterministic selection of the fallback (US-021, AC2): `pyxis` when the backend
/// accepts it, otherwise `codex_cli_rs` (allow-listed). Pure/testable, independent of the env.
pub fn originator_for(pyxis_accepted: bool) -> &'static str {
    if pyxis_accepted {
        ORIGINATOR
    } else {
        ORIGINATOR_FALLBACK
    }
}

// ───────────────── Inference constants (ChatGPT backend, Responses API) ─────────────────

pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const RESPONSES_PATH: &str = "/responses";
pub const MODELS_PATH: &str = "/models";
pub const OPENAI_BETA_SSE: &str = "responses=experimental";

/// Effective build version sent on `/models`. Release builds can inject the
/// compatibility version through `PYXIS_CODEX_CLIENT_VERSION` at compile time;
/// local builds fall back to the package version. A runtime override remains
/// available for diagnostics and compatibility probes.
pub const fn build_codex_client_version() -> &'static str {
    match option_env!("PYXIS_CODEX_CLIENT_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

pub fn codex_client_version() -> String {
    match std::env::var("PYXIS_CODEX_CLIENT_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => build_codex_client_version().to_string(),
    }
}

// ──────────────────────────────── Errors ────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid token response: {0}")]
    TokenResponse(String),
    #[error("unreadable JWT: {0}")]
    Jwt(String),
    #[error("chatgpt_account_id missing from token")]
    MissingAccountId,
    #[error("unexpected credential provider: {0:?}")]
    WrongProvider(ProviderId),
    #[error("OAuth callback: {0}")]
    Callback(String),
    #[error("OAuth state mismatch (anti-CSRF)")]
    StateMismatch,
    #[error("device flow expired (900 s)")]
    DeviceTimeout,
    #[error("device flow denied: {0}")]
    DeviceDenied(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ──────────────────────────── Wire types ────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

// ──────────────────────────── Pure builders (testable) ────────────────────────────

/// Builds the authorization URL (browser flow). Includes the
/// non-standard parameters required by the Codex backend (`id_token_add_organizations`,
/// `codex_cli_simplified_flow`).
pub fn build_authorize_url(challenge: &Secret, state: &str) -> Result<String, AuthError> {
    let mut url = url::Url::parse(AUTHORIZE_URL).map_err(|e| AuthError::Callback(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge.expose())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    Ok(url.to_string())
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
        .ok_or_else(|| AuthError::Jwt("payload absente".to_string()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| AuthError::Jwt(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| AuthError::Jwt(e.to_string()))
}

fn token_to_credential(token: TokenResponse, now_ms: u64) -> Result<OAuthCredential, AuthError> {
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

/// Result of a browser callback.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: Secret,
    pub state: Secret,
}

/// Parses the HTTP request line of the callback (`GET /auth/callback?code=...&state=... HTTP/1.1`)
/// and validates the `state` (anti-CSRF).
pub fn parse_callback_request_line(
    line: &str,
    expected_state: &str,
) -> Result<CallbackResult, AuthError> {
    let target = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| AuthError::Callback("invalid HTTP request line".to_string()))?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/auth/callback" {
        return Err(AuthError::Callback(format!("unexpected path: {path}")));
    }

    let mut code = None;
    let mut state = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }

    let state = state.ok_or_else(|| AuthError::Callback("missing state".to_string()))?;
    if state != expected_state {
        return Err(AuthError::StateMismatch);
    }
    let code = code.ok_or_else(|| AuthError::Callback("missing code".to_string()))?;
    Ok(CallbackResult {
        code: Secret::new(code),
        state: Secret::new(state),
    })
}

/// Outcome of a device-code poll.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    Pending,
    SlowDown,
    Done {
        authorization_code: Secret,
        code_verifier: Secret,
    },
}

fn device_error_code(body: &serde_json::Value) -> Option<&str> {
    body.get("errorCode")
        .or_else(|| body.get("error"))
        .and_then(|v| v.as_str())
}

/// Classifies a device-code poll response (RFC 8628 + Codex specifics).
pub fn classify_device_poll(
    status: u16,
    body: &serde_json::Value,
) -> Result<PollOutcome, AuthError> {
    if status == 200 {
        let code = body.get("authorization_code").and_then(|v| v.as_str());
        let verifier = body.get("code_verifier").and_then(|v| v.as_str());
        return match (code, verifier) {
            (Some(c), Some(v)) => Ok(PollOutcome::Done {
                authorization_code: Secret::new(c),
                // in device flow, the code_verifier comes from the SERVER, not locally.
                code_verifier: Secret::new(v),
            }),
            _ => Err(AuthError::TokenResponse(
                "device 200 without authorization_code/code_verifier".to_string(),
            )),
        };
    }
    match device_error_code(body) {
        Some("deviceauth_authorization_pending" | "authorization_pending") => {
            Ok(PollOutcome::Pending)
        }
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some("expired_token") => Err(AuthError::DeviceTimeout),
        Some(other) => Err(AuthError::DeviceDenied(other.to_string())),
        None if status == 403 || status == 404 => Ok(PollOutcome::Pending),
        None => Err(AuthError::DeviceDenied(format!("http {status}"))),
    }
}

/// SSE inference headers for a ChatGPT subscription credential. The
/// `chatgpt-account-id` (derived from the JWT) is required to route to the account.
pub fn responses_request(cred: &OAuthCredential) -> Result<ProviderRequestAuth, AuthError> {
    let mut headers = auth_headers(cred)?;
    headers.push(("OpenAI-Beta".to_string(), Secret::new(OPENAI_BETA_SSE)));
    headers.push(("accept".to_string(), Secret::new("text/event-stream")));
    headers.push(("content-type".to_string(), Secret::new("application/json")));
    Ok(ProviderRequestAuth {
        url: format!("{CHATGPT_BASE_URL}{RESPONSES_PATH}"),
        headers,
    })
}

/// Model catalog discovery request (`GET /models`). The backend
/// returns the models accessible TO THE connected ACCOUNT (`available_in_plans` field
/// already applied), filtered by the current build's `client_version`.
pub fn models_request(cred: &OAuthCredential) -> Result<ProviderRequestAuth, AuthError> {
    let mut headers = auth_headers(cred)?;
    headers.push(("accept".to_string(), Secret::new("application/json")));
    let mut url = url::Url::parse(&format!("{CHATGPT_BASE_URL}{MODELS_PATH}"))
        .map_err(|e| AuthError::Callback(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_version", &codex_client_version());
    Ok(ProviderRequestAuth {
        url: url.to_string(),
        headers,
    })
}

/// Identification headers common to every Codex backend request.
fn auth_headers(cred: &OAuthCredential) -> Result<Vec<(String, Secret)>, AuthError> {
    if cred.provider != ProviderId::OpenAiChatGpt {
        return Err(AuthError::WrongProvider(cred.provider));
    }
    let account_id = cred
        .account_id
        .as_deref()
        .ok_or(AuthError::MissingAccountId)?;
    Ok(vec![
        (
            "Authorization".to_string(),
            Secret::new(format!("Bearer {}", cred.access.expose())),
        ),
        ("chatgpt-account-id".to_string(), Secret::new(account_id)),
        ("originator".to_string(), Secret::new(originator())),
    ])
}

// ──────────────────────────── Network (token exchange / refresh) ────────────────────────────

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

// ──────────────────────────── Browser flow (PKCE + local callback server) ────────────────────────────

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Interactive login: opens the browser, waits for the callback on `127.0.0.1:1455`,
/// exchanges the code. Opening the browser is best-effort, and this crate does not
/// decide what a failure looks like: the caller receives the URL through
/// `on_auth_url` and prints it itself. That is FR-15 (US-020): only a binary writes
/// on a process output, because only a binary knows whether a TUI owns the
/// terminal.
pub async fn login_browser_with_notice<F>(
    client: &reqwest::Client,
    on_auth_url: F,
) -> Result<OAuthCredential, AuthError>
where
    F: FnOnce(&str, bool),
{
    let pkce = Pkce::generate();
    let state = random_state();
    let url = build_authorize_url(&pkce.challenge, &state)?;

    // bind BEFORE opening the browser (otherwise a race on the callback)
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await?;

    let opened = open::that(&url).is_ok();
    on_auth_url(&url, opened);

    let cb = tokio::time::timeout(CALLBACK_TIMEOUT, accept_callback(&listener, &state))
        .await
        .map_err(|_| AuthError::Callback("OAuth callback expired".to_string()))??;
    exchange_code(client, &cb.code, &pkce.verifier, REDIRECT_URI, now_ms()).await
}

const SUCCESS_BODY: &str = "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;background:#0b0b0b;color:#eaeaea;display:grid;place-items:center;height:100vh\"><div><h2>Pyxis connected</h2><p>You can close this tab.</p></div></body>";

/// Accepts connections until a valid `/auth/callback` callback is received.
/// Irrelevant requests (favicon, etc.) get a 404 and the loop goes on.
async fn accept_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> Result<CallbackResult, AuthError> {
    accept_callback_with_read_timeout(listener, expected_state, CALLBACK_READ_TIMEOUT).await
}

async fn accept_callback_with_read_timeout(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
    read_timeout: Duration,
) -> Result<CallbackResult, AuthError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = [0u8; 2048];
        let n = match tokio::time::timeout(read_timeout, sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let line = req.lines().next().unwrap_or("");

        match parse_callback_request_line(line, expected_state) {
            Ok(cb) => {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    SUCCESS_BODY.len(),
                    SUCCESS_BODY
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                return Ok(cb);
            }
            // Irrelevant request: return 404 and keep listening.
            Err(AuthError::Callback(_)) => {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await;
            }
            // State mismatch or another callback error: stop cleanly.
            Err(e) => {
                let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                return Err(e);
            }
        }
    }
}

// ──────────────────────────── Device-code flow (headless) ────────────────────────────

/// Information to present to the user for the device flow.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub user_code: Secret,
    pub verification_uri: String,
}

/// Internal poll state (separate from the user-facing display).
#[derive(Debug, Clone)]
pub struct DeviceAuthState {
    device_auth_id: Secret,
    user_code: Secret,
    interval: u64,
}

/// Starts the device flow: returns the state to poll + the info to display.
pub async fn start_device(
    client: &reqwest::Client,
) -> Result<(DeviceAuthState, DeviceAuth), AuthError> {
    let v: serde_json::Value = client
        .post(DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let field = |name: &str| {
        v.get(name)
            .and_then(|x| x.as_str())
            .map(Secret::new)
            .ok_or_else(|| AuthError::TokenResponse(format!("missing {name}")))
    };
    let device_auth_id = field("device_auth_id")?;
    let user_code = field("user_code")?;
    let interval = v
        .get("interval")
        .and_then(|x| x.as_u64())
        .unwrap_or(5)
        .max(1);

    let display = DeviceAuth {
        user_code: user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.to_string(),
    };
    Ok((
        DeviceAuthState {
            device_auth_id,
            user_code,
            interval,
        },
        display,
    ))
}

/// Polls until authorization, `slow_down`, or timeout (900 s). Final exchange through
/// `DEVICE_REDIRECT_URI`.
pub async fn poll_device(
    client: &reqwest::Client,
    st: &DeviceAuthState,
) -> Result<OAuthCredential, AuthError> {
    let start = tokio::time::Instant::now();
    let mut interval = st.interval;

    loop {
        if start.elapsed() >= DEVICE_CODE_TIMEOUT {
            return Err(AuthError::DeviceTimeout);
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let resp = client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": st.device_auth_id.expose(),
                "user_code": st.user_code.expose(),
            }))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        match classify_device_poll(status, &body)? {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = interval.saturating_add(5),
            PollOutcome::Done {
                authorization_code,
                code_verifier,
            } => {
                return exchange_code(
                    client,
                    &authorization_code,
                    &code_verifier,
                    DEVICE_REDIRECT_URI,
                    now_ms(),
                )
                .await;
            }
        }
    }
}

// ──────────────────────────── Clock ────────────────────────────

/// Now, in epoch ms (source of `expires_at`).
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ──────────────────────────────── Tests ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn extract_account_id_reads_custom_claim() {
        let jwt = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_42" }
        }));
        assert_eq!(extract_account_id(&jwt).unwrap(), "acct_42");
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
    fn authorize_url_contains_required_params() {
        let url = build_authorize_url(&Secret::new("CHAL"), "STATE123").unwrap();
        for needle in [
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge=CHAL",
            "code_challenge_method=S256",
            "state=STATE123",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=pyxis",
            "scope=openid",
        ] {
            assert!(url.contains(needle), "param absent: {needle}\n{url}");
        }
        // encoded redirect_uri
        assert!(url.contains("redirect_uri=http"));
    }

    #[test]
    fn default_catalog_client_version_is_derived_from_the_build() {
        assert_eq!(
            build_codex_client_version(),
            option_env!("PYXIS_CODEX_CLIENT_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn callback_parses_code_and_validates_state() {
        let line = "GET /auth/callback?code=abc123&state=s1 HTTP/1.1";
        let cb = parse_callback_request_line(line, "s1").unwrap();
        assert_eq!(cb.code.expose(), "abc123");

        assert!(matches!(
            parse_callback_request_line("GET /auth/callback2?code=abc123&state=s1 HTTP/1.1", "s1"),
            Err(AuthError::Callback(_))
        ));
        assert!(matches!(
            parse_callback_request_line(
                "GET /auth/callback/extra?code=abc123&state=s1 HTTP/1.1",
                "s1"
            ),
            Err(AuthError::Callback(_))
        ));
        // wrong state -> CSRF
        assert!(matches!(
            parse_callback_request_line(line, "WRONG"),
            Err(AuthError::StateMismatch)
        ));
        // irrelevant request -> Callback error (the loop 404s and goes on)
        assert!(matches!(
            parse_callback_request_line("GET /favicon.ico HTTP/1.1", "s1"),
            Err(AuthError::Callback(_))
        ));
    }

    #[test]
    fn device_poll_classification() {
        let null = serde_json::Value::Null;
        for (status, body) in [
            (403, null.clone()),
            (404, null),
            (
                400,
                serde_json::json!({"errorCode":"deviceauth_authorization_pending"}),
            ),
            (400, serde_json::json!({"error":"authorization_pending"})),
        ] {
            assert!(matches!(
                classify_device_poll(status, &body),
                Ok(PollOutcome::Pending)
            ));
        }
        assert!(matches!(
            classify_device_poll(400, &serde_json::json!({"errorCode":"slow_down"})),
            Ok(PollOutcome::SlowDown)
        ));
        assert!(matches!(
            classify_device_poll(403, &serde_json::json!({"errorCode":"access_denied"})),
            Err(AuthError::DeviceDenied(e)) if e == "access_denied"
        ));
        assert!(matches!(
            classify_device_poll(404, &serde_json::json!({"error":"expired_token"})),
            Err(AuthError::DeviceTimeout)
        ));
        assert!(matches!(
            classify_device_poll(200, &serde_json::json!({"authorization_code":"C"})),
            Err(AuthError::TokenResponse(_))
        ));

        let done = classify_device_poll(
            200,
            &serde_json::json!({"authorization_code":"C","code_verifier":"V"}),
        )
        .unwrap();
        let PollOutcome::Done {
            authorization_code,
            code_verifier,
        } = done
        else {
            unreachable!("a complete 200 is a Done")
        };
        assert_eq!(authorization_code.expose(), "C");
        assert_eq!(code_verifier.expose(), "V");
    }

    #[test]
    fn token_to_credential_sets_provider_and_sliding_expiry() {
        let jwt = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_9" }
        }));
        let token = TokenResponse {
            access_token: jwt,
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

    #[test]
    fn responses_request_has_proprietary_headers() {
        let cred = OAuthCredential {
            provider: ProviderId::OpenAiChatGpt,
            access: Secret::new("AT"),
            refresh: Secret::new("RT"),
            expires_at: 0,
            account_id: Some("acct_7".into()),
        };
        let spec = responses_request(&cred).unwrap();
        assert_eq!(spec.url, "https://chatgpt.com/backend-api/codex/responses");
        let h: std::collections::HashMap<_, _> = spec
            .header_pairs()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(h["Authorization"], "Bearer AT");
        assert_eq!(h["chatgpt-account-id"], "acct_7");
        assert_eq!(h["originator"], "pyxis");
        assert_eq!(h["OpenAI-Beta"], "responses=experimental");
    }

    #[test]
    fn responses_request_rejects_wrong_provider() {
        let cred = OAuthCredential {
            provider: ProviderId::Anthropic,
            access: Secret::new("AT"),
            refresh: Secret::new("RT"),
            expires_at: 0,
            account_id: Some("acct_7".into()),
        };
        assert!(matches!(
            responses_request(&cred),
            Err(AuthError::WrongProvider(ProviderId::Anthropic))
        ));
    }

    #[test]
    fn debug_output_redacts_oauth_transients() {
        let cred = OAuthCredential {
            provider: ProviderId::OpenAiChatGpt,
            access: Secret::new("AT_SECRET"),
            refresh: Secret::new("RT_SECRET"),
            expires_at: 0,
            account_id: Some("acct_7".into()),
        };
        let spec = responses_request(&cred).unwrap();
        let spec_dbg = format!("{spec:?}");
        assert!(!spec_dbg.contains("AT_SECRET"));
        assert!(!spec_dbg.contains("acct_7"));
        assert!(spec_dbg.contains("Secret(***)"));

        let configured = ProviderRequestAuth {
            url: "https://example.test/responses?api-key=QUERY_SECRET#FRAGMENT_SECRET".into(),
            headers: vec![("x-api-key".into(), Secret::new("HEADER_SECRET"))],
        };
        let configured_dbg = format!("{configured:?}");
        assert!(!configured_dbg.contains("QUERY_SECRET"));
        assert!(!configured_dbg.contains("FRAGMENT_SECRET"));
        assert!(!configured_dbg.contains("HEADER_SECRET"));
        assert!(configured_dbg.contains("x-api-key"));

        let cb = CallbackResult {
            code: Secret::new("CODE_SECRET"),
            state: Secret::new("STATE_SECRET"),
        };
        let cb_dbg = format!("{cb:?}");
        assert!(!cb_dbg.contains("CODE_SECRET"));
        assert!(!cb_dbg.contains("STATE_SECRET"));

        let done = PollOutcome::Done {
            authorization_code: Secret::new("AUTH_CODE_SECRET"),
            code_verifier: Secret::new("VERIFIER_SECRET"),
        };
        let done_dbg = format!("{done:?}");
        assert!(!done_dbg.contains("AUTH_CODE_SECRET"));
        assert!(!done_dbg.contains("VERIFIER_SECRET"));

        let st = DeviceAuthState {
            device_auth_id: Secret::new("DEVICE_SECRET"),
            user_code: Secret::new("USER_SECRET"),
            interval: 5,
        };
        let st_dbg = format!("{st:?}");
        assert!(!st_dbg.contains("DEVICE_SECRET"));
        assert!(!st_dbg.contains("USER_SECRET"));

        let display = DeviceAuth {
            user_code: Secret::new("DISPLAY_CODE_SECRET"),
            verification_uri: DEVICE_VERIFICATION_URI.into(),
        };
        let display_dbg = format!("{display:?}");
        assert!(!display_dbg.contains("DISPLAY_CODE_SECRET"));
    }

    #[tokio::test]
    async fn callback_read_timeout_ignores_silent_socket() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            accept_callback_with_read_timeout(&listener, "s1", Duration::from_millis(20)).await
        });

        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut good = tokio::net::TcpStream::connect(addr).await.unwrap();
        good.write_all(b"GET /auth/callback?code=abc123&state=s1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let cb = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(cb.code.expose(), "abc123");
    }

    // US-021 AC2: selection of the `originator` fallback. `pyxis` by default;
    // `codex_cli_rs` when the backend rejects `pyxis` (to be settled live).
    #[test]
    fn originator_fallback_selection() {
        assert_eq!(originator_for(true), "pyxis");
        assert_eq!(originator_for(false), "codex_cli_rs");
        assert_eq!(ORIGINATOR_FALLBACK, "codex_cli_rs");
        // env not set -> default `pyxis` (the live run will override it when needed).
        assert_eq!(originator(), "pyxis");
    }

    #[test]
    fn responses_request_without_account_id_errors() {
        let cred = OAuthCredential {
            provider: ProviderId::OpenAiChatGpt,
            access: Secret::new("AT"),
            refresh: Secret::new("RT"),
            expires_at: 0,
            account_id: None,
        };
        assert!(matches!(
            responses_request(&cred),
            Err(AuthError::MissingAccountId)
        ));
    }
}
