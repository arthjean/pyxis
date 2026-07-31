//! Network-level proof of the MCP OAuth flow: discovery walks the two metadata
//! documents (RFC 9728 then RFC 8414), dynamic registration issues a client id
//! (RFC 7591), and a refresh returns a rotated credential.
//!
//! The server is a hand-written HTTP/1.1 responder rather than a real
//! authorization server: what is under test is OUR client (which URLs it tries,
//! in which order, what it refuses, what it sends), so the answers are fixed and
//! the assertions are made on the requests that actually arrived.
//!
//! The browser round trip is deliberately not exercised here: it would open a
//! real browser. Its two halves are covered elsewhere, by
//! `parse_callback_request_line` (state check, code extraction) and by the
//! authorize-URL unit test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;

use agent_auth::oauth::mcp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Paths the fake server saw, in order.
type Seen = Arc<Mutex<Vec<String>>>;

async fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|pos| pos + 4)
        {
            break pos;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let length: usize = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split_once(':'))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let target = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    Some((
        target,
        String::from_utf8_lossy(&buf[head_end..]).to_string(),
    ))
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// How the fake server should misbehave, so the client's refusals can be
/// exercised against a real socket rather than a hand-built struct.
#[derive(Clone, Copy, Default)]
struct Quirk {
    /// Claim a resource on another host: the confused-deputy shape, where the
    /// user consents at a genuine identity provider for a third party and the
    /// token comes back to whoever asked for it.
    foreign_resource: bool,
    /// Answer the authorization-server document with a body far over the cap.
    oversized_metadata: bool,
}

/// Fake MCP resource + authorization server on one loopback port. Plain http is
/// accepted by the client for loopback only, which is exactly this case.
async fn spawn_server() -> (String, Seen, Arc<Mutex<Vec<String>>>) {
    spawn_server_with(Quirk::default()).await
}

async fn spawn_server_with(quirk: Quirk) -> (String, Seen, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_task = Arc::clone(&seen);
    let bodies_task = Arc::clone(&bodies);
    let base_task = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen_task);
            let bodies = Arc::clone(&bodies_task);
            let base = base_task.clone();
            tokio::spawn(async move {
                let Some((target, body)) = read_request(&mut stream).await else {
                    return;
                };
                if let Ok(mut seen) = seen.lock() {
                    seen.push(target.clone());
                }
                if !body.is_empty()
                    && let Ok(mut bodies) = bodies.lock()
                {
                    bodies.push(body.clone());
                }
                match target.as_str() {
                    // Path-aware protected-resource document, the form tried first.
                    "/.well-known/oauth-protected-resource/mcp" => {
                        let resource = if quirk.foreign_resource {
                            "https://graph.microsoft.com".to_string()
                        } else {
                            format!("{base}/mcp")
                        };
                        respond(
                            &mut stream,
                            "200 OK",
                            &serde_json::json!({
                                "resource": resource,
                                "authorization_servers": [base],
                            })
                            .to_string(),
                        )
                        .await;
                    }
                    "/.well-known/oauth-authorization-server" => {
                        let mut document = serde_json::json!({
                            "issuer": base,
                            "authorization_endpoint": format!("{base}/authorize"),
                            "token_endpoint": format!("{base}/token"),
                            "registration_endpoint": format!("{base}/register"),
                            "scopes_supported": ["mcp.read", "mcp.write"],
                            "code_challenge_methods_supported": ["S256"],
                        });
                        if quirk.oversized_metadata {
                            document["padding"] = serde_json::Value::String("x".repeat(256 * 1024));
                        }
                        respond(&mut stream, "200 OK", &document.to_string()).await;
                    }
                    "/register" => {
                        respond(
                            &mut stream,
                            "201 Created",
                            &serde_json::json!({"client_id": "issued-client-id"}).to_string(),
                        )
                        .await;
                    }
                    "/token" => {
                        respond(
                            &mut stream,
                            "200 OK",
                            &serde_json::json!({
                                "access_token": "fresh-access",
                                "refresh_token": "rotated-refresh",
                                "expires_in": 3600,
                                "token_type": "Bearer",
                            })
                            .to_string(),
                        )
                        .await;
                    }
                    _ => respond(&mut stream, "404 Not Found", "{}").await,
                }
            });
        }
    });
    (base, seen, bodies)
}

#[tokio::test]
async fn discovery_walks_the_resource_then_the_authorization_server() {
    let (base, seen, _) = spawn_server().await;
    let client = reqwest::Client::new();

    let (metadata, resource) = mcp::discover(&client, &format!("{base}/mcp"), None)
        .await
        .expect("discovery must succeed");

    assert_eq!(metadata.authorization_endpoint, format!("{base}/authorize"));
    assert_eq!(metadata.token_endpoint, format!("{base}/token"));
    assert_eq!(
        metadata.registration_endpoint.as_deref(),
        Some(format!("{base}/register").as_str())
    );
    assert_eq!(metadata.scopes_supported, vec!["mcp.read", "mcp.write"]);
    // RFC 8707: the resource the token will be bound to comes from the document,
    // not from a guess.
    assert_eq!(resource.as_deref(), Some(format!("{base}/mcp").as_str()));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.first().map(String::as_str),
        Some("/.well-known/oauth-protected-resource/mcp"),
        "the path-aware form is tried first: {seen:?}"
    );
    assert!(
        seen.contains(&"/.well-known/oauth-authorization-server".to_string()),
        "{seen:?}"
    );
}

#[tokio::test]
async fn dynamic_registration_declares_a_public_client() {
    let (base, _, bodies) = spawn_server().await;
    let client = reqwest::Client::new();

    let client_id = mcp::register_client(
        &client,
        &format!("{base}/register"),
        "http://127.0.0.1:7777/pyxis/mcp/callback",
    )
    .await
    .expect("registration must succeed");
    assert_eq!(client_id, "issued-client-id");

    let body: serde_json::Value =
        serde_json::from_str(bodies.lock().unwrap().first().expect("a body was sent")).unwrap();
    // A CLI has no secret to keep: it registers as a public client, and the
    // redirect it declares is the loopback one it is actually listening on.
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert_eq!(
        body["redirect_uris"],
        serde_json::json!(["http://127.0.0.1:7777/pyxis/mcp/callback"])
    );
    assert_eq!(
        body["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
}

#[tokio::test]
async fn a_refresh_returns_the_rotated_credential() {
    let (base, seen, _) = spawn_server().await;
    let client = reqwest::Client::new();

    let stale = mcp::McpOAuthCredential {
        server: "remote".to_string(),
        client_id: "issued-client-id".to_string(),
        access: agent_auth::Secret::new("stale-access"),
        refresh: Some(agent_auth::Secret::new("old-refresh")),
        expires_at: 1,
        token_endpoint: format!("{base}/token"),
        resource: Some(format!("{base}/mcp")),
        scopes: Some("mcp.read".to_string()),
    };
    assert!(stale.needs_refresh(1_000_000));

    let refreshed = mcp::refresh(&client, &stale, 1_000_000)
        .await
        .expect("refresh must succeed");
    assert_eq!(refreshed.access.expose(), "fresh-access");
    // The rotated token replaces the old one: reusing the old one would fail on
    // the next refresh.
    assert_eq!(
        refreshed.refresh.as_ref().map(agent_auth::Secret::expose),
        Some("rotated-refresh")
    );
    assert_eq!(refreshed.expires_at, 1_000_000 + 3_600_000);
    assert!(!refreshed.needs_refresh(1_000_000));
    assert_eq!(seen.lock().unwrap().clone(), vec!["/token".to_string()]);
}

#[tokio::test]
async fn a_credential_without_a_refresh_token_says_to_log_in_again() {
    let (base, _, _) = spawn_server().await;
    let client = reqwest::Client::new();
    let cred = mcp::McpOAuthCredential {
        server: "remote".to_string(),
        client_id: "c".to_string(),
        access: agent_auth::Secret::new("a"),
        refresh: None,
        expires_at: 1,
        token_endpoint: format!("{base}/token"),
        resource: None,
        scopes: None,
    };
    let err = mcp::refresh(&client, &cred, 1_000_000)
        .await
        .expect_err("no refresh token");
    assert!(err.to_string().contains("log in again"), "{err}");
}

#[tokio::test]
async fn a_non_loopback_plain_http_endpoint_is_refused_before_any_request() {
    let client = reqwest::Client::new();
    let err = mcp::discover(&client, "http://mcp.example.com/mcp", None)
        .await
        .expect_err("plain http must be refused");
    assert!(err.to_string().contains("not https"), "{err}");
}

/// RFC 9728 §3.3. Without this check the RFC 8707 binding is chosen by the
/// server: it names a third-party resource, the user consents at that party's
/// real identity provider, and the token it grants is then sent to the server
/// that asked for it.
#[tokio::test]
async fn a_resource_metadata_claiming_another_host_is_refused() {
    let (base, seen, _) = spawn_server_with(Quirk {
        foreign_resource: true,
        ..Quirk::default()
    })
    .await;
    let client = reqwest::Client::new();

    let err = mcp::discover(&client, &format!("{base}/mcp"), None)
        .await
        .expect_err("a foreign resource must end the flow");
    let message = err.to_string();
    assert!(message.contains("graph.microsoft.com"), "{message}");
    assert!(message.contains("does not cover"), "{message}");
    // The refusal is final: discovery must not fall through to the origin, which
    // would turn an attempted redirection into a silent success.
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.contains(&"/.well-known/oauth-authorization-server".to_string()),
        "{seen:?}"
    );
}

/// The cap is enforced on what arrives, not on what was already allocated: an
/// unauthenticated party gets to answer discovery, so an oversized document is a
/// refusal rather than a memory bill.
#[tokio::test]
async fn an_oversized_metadata_document_is_refused() {
    let (base, _, _) = spawn_server_with(Quirk {
        oversized_metadata: true,
        ..Quirk::default()
    })
    .await;
    let client = reqwest::Client::new();

    let err = mcp::discover(&client, &format!("{base}/mcp"), None)
        .await
        .expect_err("an oversized document is not a document");
    assert!(
        err.to_string().contains("no OAuth authorization server"),
        "{err}"
    );
}

#[tokio::test]
async fn a_hint_pointing_off_origin_is_ignored() {
    let (base, seen, _) = spawn_server().await;
    let client = reqwest::Client::new();

    // A compromised server cannot delegate its own protection to a host it does
    // not own: the hint is dropped and discovery falls back to the origin.
    let (metadata, _) = mcp::discover(
        &client,
        &format!("{base}/mcp"),
        Some("https://attacker.example.com/.well-known/oauth-protected-resource"),
    )
    .await
    .expect("discovery must fall back to the origin");
    assert_eq!(metadata.token_endpoint, format!("{base}/token"));
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|path| !path.contains("attacker")),
        "{seen:?}"
    );
}
