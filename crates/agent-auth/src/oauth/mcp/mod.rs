//! OAuth 2.1 for remote MCP servers (MCP authorization, spec 2025-06-18).
//!
//! A remote MCP server that refuses an anonymous connection answers `401` with a
//! `WWW-Authenticate` header naming its protected-resource metadata. From there
//! the client discovers an authorization server, registers itself if it has no
//! client id, and runs an authorization-code flow with PKCE. Four RFCs are in
//! play and each one is load-bearing here:
//!
//! - **RFC 9728** protected-resource metadata: which authorization server guards
//!   this MCP endpoint.
//! - **RFC 8414** authorization-server metadata: where to send the user and where
//!   to exchange the code.
//! - **RFC 7591** dynamic client registration: how a CLI with no pre-registered
//!   client id obtains one.
//! - **RFC 8707** resource indicators: the token is bound to THIS MCP server, so a
//!   server cannot replay it against another resource.
//!
//! Security decisions that are not negotiable here, because a hostile or merely
//! sloppy server would otherwise turn discovery into an exfiltration primitive:
//!
//! 1. **Every discovered endpoint must be https**, with the single exception of a
//!    loopback host (the local development case). A `http://evil.example` handed
//!    back by discovery would receive the authorization code in clear.
//! 2. **The authorization server must be named by the resource itself.** We never
//!    follow an authorization server that a `WWW-Authenticate` header alone
//!    proposes without the protected-resource document backing it.
//! 3. **The document must claim the resource it protects** (RFC 9728 §3.3). This
//!    is what actually makes the RFC 8707 binding worth anything: without it a
//!    server declares someone else's `resource`, the user consents at a real
//!    identity provider for a third party, and the token comes back to be sent
//!    to the server that asked for it. The resource is checked against the MCP
//!    endpoint, and a mismatch ends the flow.
//! 4. **The redirect URI is loopback with an ephemeral port** (RFC 8252 §7.3),
//!    bound before the browser opens so nothing else can claim it.
//! 5. **`state` is verified** on the callback, and the code never leaves the
//!    process except toward the discovered token endpoint.
//! 6. **Every body is read under a cap**, before it is allocated rather than
//!    after: discovery talks to a party that has not authenticated yet.

use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::oauth::openai_chatgpt::AuthError;
use crate::oauth::pkce::Pkce;
use crate::{Credential, Secret};

/// Bound of one discovery or token request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound of the whole browser round trip.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Path of the local callback. Fixed; only the port is ephemeral.
const CALLBACK_PATH: &str = "/pyxis/mcp/callback";
/// Refresh margin: a token about to expire is refreshed rather than used.
const REFRESH_MARGIN_MS: u64 = 60_000;
/// Bound of a discovery document: metadata is small, a multi-megabyte answer is
/// an attack, not a document.
const MAX_METADATA_BYTES: usize = 128 * 1024;
/// Bound of a registration or token response. Tighter than a metadata document:
/// it carries a handful of strings.
const MAX_TOKEN_BYTES: usize = 64 * 1024;

const SUCCESS_BODY: &str = "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;background:#0b0b0b;color:#eaeaea;display:grid;place-items:center;height:100vh\"><div><h2>MCP server connected</h2><p>You can close this tab.</p></div></body>";

/// Keyring account of one MCP server's credential. Namespaced so it can never
/// collide with a provider credential.
pub fn account_key(server: &str) -> String {
    format!("mcp-oauth:{server}")
}

/// OAuth credential of one MCP server.
///
/// Everything needed to refresh without redoing discovery is stored: a refresh
/// must not depend on a server still serving the same metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthCredential {
    /// Logical name of the MCP server, as written in the config.
    pub server: String,
    /// Client id: either configured by the user or obtained by registration.
    pub client_id: String,
    pub access: Secret,
    pub refresh: Option<Secret>,
    /// Absolute ms timestamp. `0` when the server declared no expiry.
    pub expires_at: u64,
    /// Where to exchange a refresh token.
    pub token_endpoint: String,
    /// RFC 8707 resource this token is bound to.
    pub resource: Option<String>,
    /// Space-separated scopes the token was granted.
    pub scopes: Option<String>,
}

impl McpOAuthCredential {
    /// Is the token expired, or close enough that a call would race the expiry?
    pub fn needs_refresh(&self, now_ms: u64) -> bool {
        // A server that declares no expiry is taken at its word: it is refreshed
        // only once a call actually fails.
        self.expires_at != 0 && now_ms.saturating_add(REFRESH_MARGIN_MS) >= self.expires_at
    }
}

/// What discovery produced, and what a login needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthServerMetadata {
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// Inputs of a login, taken from the server entry in the config.
#[derive(Debug, Clone, Default)]
pub struct McpOAuthRequest {
    /// Logical server name, used as the keyring account and in messages.
    pub server: String,
    /// The MCP endpoint itself.
    pub url: String,
    /// Client id the user configured. Absent: dynamic registration is attempted.
    pub client_id: Option<String>,
    /// Scopes the user asked for. Empty: what the server advertises is used.
    pub scopes: Vec<String>,
    /// RFC 8707 resource. Absent: the MCP url is used, which is the spec default.
    pub resource: Option<String>,
}

mod discovery;

use discovery::read_json;
pub use discovery::{discover, resource_metadata_hint};

// Wire shapes of the two token-issuing endpoints. They belong here rather
// than with discovery: nothing is discovered from them, they are the answer
// to a request this module makes.
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

// ───────────────────────────── Registration (RFC 7591) ─────────────────────────────

/// Registers this CLI as a public OAuth client. Returns the issued client id.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<String, AuthError> {
    let body = serde_json::json!({
        "client_name": "Pyxis",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        // Public client: there is no secret a CLI could keep.
        "token_endpoint_auth_method": "none",
    });
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client.post(registration_endpoint).json(&body).send(),
    )
    .await
    .map_err(|_| AuthError::Callback("client registration timed out".to_string()))??;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Callback(format!(
            "client registration refused: HTTP {status}"
        )));
    }
    let registered: RegistrationResponse = read_json(response, MAX_TOKEN_BYTES).await?;
    Ok(registered.client_id)
}

// ───────────────────────────── Login ─────────────────────────────

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Builds the authorization URL. `resource` implements RFC 8707: the token the
/// user is about to grant is bound to this MCP server and to no other.
fn build_authorize_url(
    metadata: &AuthServerMetadata,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scopes: &[String],
    resource: Option<&str>,
) -> Result<String, AuthError> {
    let mut url = url::Url::parse(&metadata.authorization_endpoint)
        .map_err(|e| AuthError::Callback(format!("authorization endpoint: {e}")))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        if !scopes.is_empty() {
            query.append_pair("scope", &scopes.join(" "));
        }
        if let Some(resource) = resource {
            query.append_pair("resource", resource);
        }
    }
    Ok(url.to_string())
}

/// Interactive login against a remote MCP server.
///
/// The caller receives the authorization URL through `on_auth_url` and decides
/// how to show it: only a binary knows whether a TUI owns the terminal (FR-15).
pub async fn login<F>(
    client: &reqwest::Client,
    request: &McpOAuthRequest,
    now_ms: u64,
    on_auth_url: F,
) -> Result<McpOAuthCredential, AuthError>
where
    F: FnOnce(&str, bool),
{
    let (metadata, discovered_resource) = discover(client, &request.url, None).await?;
    // Bound BEFORE the browser opens, on an ephemeral loopback port (RFC 8252
    // §7.3): nothing else can claim the redirect in between.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let client_id = match &request.client_id {
        Some(client_id) => client_id.clone(),
        None => {
            let Some(registration) = metadata.registration_endpoint.as_deref() else {
                return Err(AuthError::Callback(format!(
                    "MCP server \"{}\" needs an OAuth client id: its authorization server offers no dynamic registration",
                    request.server
                )));
            };
            register_client(client, registration, &redirect_uri).await?
        }
    };

    let scopes = if request.scopes.is_empty() {
        metadata.scopes_supported.clone()
    } else {
        request.scopes.clone()
    };
    let resource = request
        .resource
        .clone()
        .or(discovered_resource)
        .or_else(|| Some(request.url.clone()));

    let pkce = Pkce::generate();
    let state = random_state();
    let url = build_authorize_url(
        &metadata,
        &client_id,
        &redirect_uri,
        &pkce.challenge,
        &state,
        &scopes,
        resource.as_deref(),
    )?;

    let opened = open::that(&url).is_ok();
    on_auth_url(&url, opened);

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, accept_callback(&listener, &state))
        .await
        .map_err(|_| AuthError::Callback("OAuth callback expired".to_string()))??;

    let token = exchange_code(
        client,
        &metadata.token_endpoint,
        &client_id,
        &code,
        &pkce.verifier,
        &redirect_uri,
        resource.as_deref(),
    )
    .await?;
    Ok(credential_from_token(
        token,
        request.server.clone(),
        client_id,
        metadata.token_endpoint,
        resource,
        now_ms,
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one token request: every field is a distinct wire parameter"
)]
async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    resource: Option<&str>,
) -> Result<TokenResponse, AuthError> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    if let Some(resource) = resource {
        form.push(("resource", resource));
    }
    let response =
        tokio::time::timeout(HTTP_TIMEOUT, client.post(token_endpoint).form(&form).send())
            .await
            .map_err(|_| AuthError::Callback("token exchange timed out".to_string()))??;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::TokenResponse(format!("HTTP {status}")));
    }
    read_json(response, MAX_TOKEN_BYTES).await
}

/// Refreshes a credential. The refresh token may rotate, so the returned
/// credential is what must be written back.
pub async fn refresh(
    client: &reqwest::Client,
    cred: &McpOAuthCredential,
    now_ms: u64,
) -> Result<McpOAuthCredential, AuthError> {
    let Some(refresh_token) = cred.refresh.as_ref() else {
        return Err(AuthError::TokenResponse(format!(
            "MCP server \"{}\" has no refresh token: log in again",
            cred.server
        )));
    };
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("client_id", cred.client_id.as_str()),
        ("refresh_token", refresh_token.expose()),
    ];
    if let Some(resource) = cred.resource.as_deref() {
        form.push(("resource", resource));
    }
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client.post(&cred.token_endpoint).form(&form).send(),
    )
    .await
    .map_err(|_| AuthError::Callback("token refresh timed out".to_string()))??;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::TokenResponse(format!("HTTP {status}")));
    }
    let token: TokenResponse = read_json(response, MAX_TOKEN_BYTES).await?;
    let mut refreshed = credential_from_token(
        token,
        cred.server.clone(),
        cred.client_id.clone(),
        cred.token_endpoint.clone(),
        cred.resource.clone(),
        now_ms,
    );
    // What the server did not rotate, we keep. `or_else` and not
    // `get_or_insert_with(|| ...unwrap_or_default())`: the latter turned a
    // credential that never had scopes into one carrying `Some("")`.
    refreshed.refresh = refreshed.refresh.or_else(|| cred.refresh.clone());
    refreshed.scopes = refreshed.scopes.or_else(|| cred.scopes.clone());
    Ok(refreshed)
}

fn credential_from_token(
    token: TokenResponse,
    server: String,
    client_id: String,
    token_endpoint: String,
    resource: Option<String>,
    now_ms: u64,
) -> McpOAuthCredential {
    let expires_at = token
        .expires_in
        .map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1_000)))
        .unwrap_or(0);
    McpOAuthCredential {
        server,
        client_id,
        access: Secret::new(token.access_token),
        refresh: token.refresh_token.map(Secret::new),
        expires_at,
        token_endpoint,
        resource,
        scopes: token.scope,
    }
}

// ───────────────────────────── Local callback ─────────────────────────────

/// Extracts the authorization code from a callback request line, after checking
/// the anti-CSRF `state`. `Err(Callback)` = not our callback, keep listening.
pub fn parse_callback_request_line(line: &str, expected_state: &str) -> Result<String, AuthError> {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        return Err(AuthError::Callback("not a GET".to_string()));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Err(AuthError::Callback(format!("unexpected path {path}")));
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(AuthError::TokenResponse(format!(
            "authorization refused: {error}"
        )));
    }
    // The state is checked BEFORE the code is looked at: a callback that does not
    // belong to this flow must not get its code read, logged or exchanged.
    if state.as_deref() != Some(expected_state) {
        return Err(AuthError::StateMismatch);
    }
    code.ok_or_else(|| AuthError::Callback("callback carries no code".to_string()))
}

async fn accept_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, AuthError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = [0u8; 4096];
        let n = match tokio::time::timeout(CALLBACK_READ_TIMEOUT, sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let line = request.lines().next().unwrap_or_default();
        match parse_callback_request_line(line, expected_state) {
            Ok(code) => {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    SUCCESS_BODY.len(),
                    SUCCESS_BODY
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                return Ok(code);
            }
            // A mismatched state or an explicit refusal ends the flow; anything
            // else (favicon, probe) gets a 404 and the loop goes on.
            Err(err @ (AuthError::StateMismatch | AuthError::TokenResponse(_))) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                    .await;
                return Err(err);
            }
            Err(_) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
                    .await;
            }
        }
    }
}

// ───────────────────────────── Storage ─────────────────────────────

/// Persists the credential of one MCP server in the OS secret store.
pub fn save(cred: &McpOAuthCredential) -> Result<(), crate::store::StoreError> {
    crate::store::save(
        &account_key(&cred.server),
        &Credential::McpOauth(cred.clone()),
    )
}

/// Reads the stored credential of one MCP server, `None` when absent.
pub fn load(server: &str) -> Result<Option<McpOAuthCredential>, crate::store::StoreError> {
    match crate::store::load(&account_key(server))? {
        Some(Credential::McpOauth(cred)) => Ok(Some(cred)),
        // Another credential shape under that key: treated as absent rather than
        // as an error, so a stale entry cannot make a server unusable forever.
        Some(_) | None => Ok(None),
    }
}

/// Forgets the credential of one MCP server (idempotent).
pub fn delete(server: &str) -> Result<(), crate::store::StoreError> {
    crate::store::delete(&account_key(server))
}

/// Returns a usable access token, refreshing and rewriting it when needed.
/// `None` when the server has no stored credential.
///
/// The keyring is reached through `spawn_blocking`: on Linux it talks D-Bus to
/// the Secret Service, which must not run on the async runtime's thread.
pub async fn access_token(
    client: &reqwest::Client,
    server: &str,
    now_ms: u64,
) -> Result<Option<String>, AuthError> {
    let owned = server.to_string();
    let stored = tokio::task::spawn_blocking(move || load(&owned))
        .await
        .map_err(|e| AuthError::Callback(format!("keyring task: {e}")))?
        .map_err(|e| AuthError::Callback(e.to_string()))?;
    let Some(cred) = stored else {
        return Ok(None);
    };
    if !cred.needs_refresh(now_ms) {
        return Ok(Some(cred.access.expose().to_string()));
    }
    let refreshed = refresh(client, &cred, now_ms).await?;
    let token = refreshed.access.expose().to_string();
    // A write failure is not fatal: the fresh token is still usable for this run.
    let _ = tokio::task::spawn_blocking(move || save(&refreshed)).await;
    Ok(Some(token))
}

/// Milliseconds since the Unix epoch, the clock every expiry here is expressed in.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: Some("https://as.example.com".to_string()),
            authorization_endpoint: "https://as.example.com/authorize".to_string(),
            token_endpoint: "https://as.example.com/token".to_string(),
            registration_endpoint: Some("https://as.example.com/register".to_string()),
            scopes_supported: vec!["mcp".to_string()],
        }
    }

    #[test]
    fn the_authorize_url_carries_pkce_and_the_resource_binding() {
        let url = build_authorize_url(
            &metadata(),
            "client-123",
            "http://127.0.0.1:5000/pyxis/mcp/callback",
            "CHALLENGE",
            "STATE",
            &["mcp".to_string(), "read".to_string()],
            Some("https://api.example.com/mcp"),
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "client-123");
        assert_eq!(query["code_challenge"], "CHALLENGE");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], "STATE");
        assert_eq!(query["scope"], "mcp read");
        // RFC 8707: the token is bound to this resource and cannot be replayed.
        assert_eq!(query["resource"], "https://api.example.com/mcp");
    }

    #[test]
    fn the_callback_checks_the_state_before_reading_the_code() {
        let line = "GET /pyxis/mcp/callback?code=SECRET&state=good HTTP/1.1";
        assert_eq!(parse_callback_request_line(line, "good").unwrap(), "SECRET");
        // Wrong state: rejected as a state mismatch, and the code is not returned.
        let err = parse_callback_request_line(line, "other").unwrap_err();
        assert!(matches!(err, AuthError::StateMismatch));
        assert!(!err.to_string().contains("SECRET"));
        // An explicit refusal by the server is surfaced.
        let denied = "GET /pyxis/mcp/callback?error=access_denied&state=good HTTP/1.1";
        assert!(
            parse_callback_request_line(denied, "good")
                .unwrap_err()
                .to_string()
                .contains("access_denied")
        );
        // Anything else is "not our callback".
        assert!(parse_callback_request_line("GET /favicon.ico HTTP/1.1", "good").is_err());
        assert!(
            parse_callback_request_line("POST /pyxis/mcp/callback?code=x&state=good", "good")
                .is_err()
        );
    }

    #[test]
    fn a_credential_is_refreshed_before_it_expires_not_after() {
        let cred = McpOAuthCredential {
            server: "srv".to_string(),
            client_id: "c".to_string(),
            access: Secret::new("at"),
            refresh: Some(Secret::new("rt")),
            expires_at: 1_000_000,
            token_endpoint: "https://as.example.com/token".to_string(),
            resource: None,
            scopes: None,
        };
        assert!(!cred.needs_refresh(1_000_000 - REFRESH_MARGIN_MS - 1));
        // Inside the margin: refreshed rather than raced.
        assert!(cred.needs_refresh(1_000_000 - REFRESH_MARGIN_MS));
        assert!(cred.needs_refresh(2_000_000));
        // No declared expiry: taken at its word.
        let eternal = McpOAuthCredential {
            expires_at: 0,
            ..cred
        };
        assert!(!eternal.needs_refresh(u64::MAX));
    }

    #[test]
    fn a_token_response_without_expiry_is_stored_without_one() {
        let cred = credential_from_token(
            TokenResponse {
                access_token: "ACCESS_TOKEN_VALUE".to_string(),
                refresh_token: None,
                expires_in: None,
                scope: None,
            },
            "srv".to_string(),
            "c".to_string(),
            "https://as.example.com/token".to_string(),
            None,
            5_000,
        );
        assert_eq!(cred.expires_at, 0);
        assert!(cred.refresh.is_none());
        // The secret never shows up in a debug rendering.
        assert!(!format!("{cred:?}").contains("ACCESS_TOKEN_VALUE"));
    }

    #[test]
    fn an_expiry_is_absolute_and_a_rotated_refresh_token_replaces_the_old_one() {
        let cred = credential_from_token(
            TokenResponse {
                access_token: "a".to_string(),
                refresh_token: Some("new-rt".to_string()),
                expires_in: Some(3_600),
                scope: Some("mcp".to_string()),
            },
            "srv".to_string(),
            "c".to_string(),
            "https://as.example.com/token".to_string(),
            Some("https://api.example.com/mcp".to_string()),
            1_000,
        );
        assert_eq!(cred.expires_at, 1_000 + 3_600_000);
        assert_eq!(cred.refresh.as_ref().map(Secret::expose), Some("new-rt"));
        assert_eq!(cred.scopes.as_deref(), Some("mcp"));
        assert_eq!(
            cred.resource.as_deref(),
            Some("https://api.example.com/mcp")
        );
    }

    #[test]
    fn the_keyring_account_is_namespaced_per_server() {
        assert_eq!(account_key("files"), "mcp-oauth:files");
        assert_ne!(account_key("files"), account_key("other"));
    }
}
