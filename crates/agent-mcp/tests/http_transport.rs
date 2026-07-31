//! US-013: a remote MCP server over Streamable HTTP connects and lists its tools
//! exactly like a stdio one.
//!
//! The server is a hand-written HTTP/1.1 responder rather than a full MCP server
//! implementation: what is under test is OUR transport adapter (headers, status
//! codes, JSON body, session id), so the responses are fixed and the assertions
//! are made on what the client actually sent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_mcp::{McpConnection, McpServerConfig, McpServerPolicy, McpToolPolicy, McpTransport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// One HTTP request as the test server saw it.
struct Request {
    headers: String,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .lines()
            .find(|line| {
                line.split(':')
                    .next()
                    .is_some_and(|key| key.eq_ignore_ascii_case(name))
            })
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim())
    }

    fn method_name(&self) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
    }

    fn id(&self) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::Value::Null)
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    // Headers first: read until the blank line.
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
    Some(Request {
        headers,
        body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
    })
}

async fn respond(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Minimal Streamable HTTP MCP server, bound on a loopback port. Answers
/// `initialize` and `tools/list`, refuses the server-to-client stream (405), and
/// records whether the bearer token reached it.
async fn spawn_server(expect_token: Option<String>) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let token_seen = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&token_seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let expect_token = expect_token.clone();
            let flag = Arc::clone(&flag);
            tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else {
                    return;
                };
                if let Some(expected) = expect_token.as_deref()
                    && request.header("authorization") == Some(&format!("Bearer {expected}"))
                {
                    flag.store(true, Ordering::SeqCst);
                }
                // The GET stream is optional in the spec.
                if request.headers.starts_with("GET ") {
                    respond(&mut stream, "405 Method Not Allowed", "", "").await;
                    return;
                }
                match request.method_name().as_deref() {
                    Some("initialize") => {
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request.id(),
                            "result": {
                                "protocolVersion": PROTOCOL_VERSION,
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "test-remote", "version": "0.0.0"}
                            }
                        })
                        .to_string();
                        respond(
                            &mut stream,
                            "200 OK",
                            "Mcp-Session-Id: test-session\r\n",
                            &body,
                        )
                        .await;
                    }
                    Some("tools/list") => {
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request.id(),
                            "result": {"tools": [
                                {
                                    "name": "remote_search",
                                    "description": "search the remote index",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {"query": {"type": "string"}},
                                        "required": ["query"]
                                    }
                                },
                                {
                                    "name": "remote_delete",
                                    "description": "delete a remote document",
                                    "inputSchema": {"type": "object", "properties": {}}
                                }
                            ]}
                        })
                        .to_string();
                        respond(&mut stream, "200 OK", "", &body).await;
                    }
                    Some("tools/call") => {
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request.id(),
                            "result": {
                                "content": [{"type": "text", "text": "3 remote hits"}],
                                "isError": false
                            }
                        })
                        .to_string();
                        respond(&mut stream, "200 OK", "", &body).await;
                    }
                    // Notifications and anything else: acknowledged.
                    _ => respond(&mut stream, "202 Accepted", "", "").await,
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), token_seen)
}

fn http_config(url: &str, bearer_token_env_var: Option<&str>) -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Http {
            url: url.to_string(),
            bearer_token_env_var: bearer_token_env_var.map(str::to_string),
            http_headers: Default::default(),
            env_http_headers: Default::default(),
            oauth: Default::default(),
        },
        policy: McpServerPolicy::default(),
        source: Default::default(),
        shadows_lower_priority: false,
    }
}

#[tokio::test]
async fn a_remote_server_connects_and_lists_its_tools() {
    let (url, _) = spawn_server(None).await;
    let conn = McpConnection::connect("remote", &http_config(&url, None))
        .await
        .expect("the handshake must succeed");
    let tools = conn.list_tools("remote").await.expect("tools listed");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool.original_name.as_str())
        .collect();
    assert_eq!(names, vec!["remote_search", "remote_delete"]);
    // Same untrusted treatment as stdio: the annotations are hints, nothing more.
    assert!(tools[0].description.contains("remote index"));
    conn.cancel().await;
}

/// The epic's definition of done: a remote server is USABLE, not merely reachable.
/// The tool goes through the same exposure path as a stdio one and its result
/// comes back through the same client.
#[tokio::test]
async fn a_remote_tool_is_exposed_filtered_and_callable() {
    let (url, _) = spawn_server(None).await;
    let conn = McpConnection::connect("remote", &http_config(&url, None))
        .await
        .unwrap();
    let listed = conn.list_tools("remote").await.unwrap();

    // US-014: the deny-list removes the destructive tool without disconnecting.
    let policy = McpToolPolicy {
        disabled: std::collections::BTreeSet::from(["remote_delete".to_string()]),
        ..McpToolPolicy::default()
    };
    let (kept, notices) = agent_mcp::filter_tools("remote", &listed, &policy);
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(kept.len(), 1);

    let client = conn.client("remote");
    let mut taken = std::collections::BTreeSet::new();
    let server_policy = McpServerPolicy {
        tools: policy.clone(),
        ..McpServerPolicy::default()
    };
    let (tools, skipped) =
        agent_mcp::dyn_tools("remote", &kept, &server_policy, &client, &mut taken);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__remote__remote_search");
    // Same trust posture as stdio: a remote result is untrusted by construction.
    assert!(tools[0].returns_untrusted());

    let outcome = client
        .call(
            "remote_search",
            Some(
                serde_json::json!({"query": "pyxis"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            /*vision*/ false,
        )
        .await
        .expect("the remote call must succeed");
    assert_eq!(outcome.text, "3 remote hits");
    assert!(!outcome.is_error);
    conn.cancel().await;
}

#[tokio::test]
async fn the_bearer_token_is_read_from_the_environment_and_never_stored() {
    let (url, token_seen) = spawn_server(Some("s3cr3t-value".to_string())).await;
    // SAFETY: a name unique to this test, written before any connection reads it.
    unsafe { std::env::set_var("PYXIS_TEST_MCP_HTTP_TOKEN", "s3cr3t-value") };
    let cfg = http_config(&url, Some("PYXIS_TEST_MCP_HTTP_TOKEN"));
    // US-013 AC3: the config carries the variable NAME, never its value.
    assert!(!format!("{cfg:?}").contains("s3cr3t-value"));

    let conn = McpConnection::connect("remote", &cfg)
        .await
        .expect("the handshake must succeed");
    conn.cancel().await;
    assert!(
        token_seen.load(Ordering::SeqCst),
        "the server must have received the Authorization header"
    );
}

#[tokio::test]
async fn a_missing_token_variable_names_it_without_leaking_anything() {
    let cfg = http_config("https://mcp.example.com/mcp", Some("PYXIS_TEST_MCP_ABSENT"));
    let err = McpConnection::connect("remote", &cfg)
        .await
        .err()
        .expect("connection must fail");
    let message = err.to_string();
    assert!(message.contains("PYXIS_TEST_MCP_ABSENT"), "{message}");
    assert!(message.contains("remote"), "{message}");
}

#[tokio::test]
async fn an_unreachable_server_fails_bounded_and_named() {
    // A port nobody listens on: bound then released, so the number is free.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = http_config(&format!("http://127.0.0.1:{port}/mcp"), None);

    let started = std::time::Instant::now();
    let err = McpConnection::connect("dead", &cfg)
        .await
        .err()
        .expect("connection must fail");
    // US-013 AC2: bounded, named, and the caller stays free to start the session.
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(err.to_string().contains("dead"), "{err}");
}
