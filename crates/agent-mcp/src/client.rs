//! MCP client: connection to a server through the stdio transport (subprocess) or
//! the Streamable HTTP transport (remote, US-013), automatic `initialize`
//! handshake, tool listing. Both transports produce the same `McpConnection`: the
//! rest of the crate never learns which one it is talking to.
//!
//! Three bounds make this file a boundary rather than a wrapper around `rmcp`:
//!
//! 1. **Startup is one deadline, not a stack of them.** `startupTimeoutMs` covers
//!    spawn, handshake, retries and `tools/list` together. A per-step bound would
//!    let the steps add up to a multiple of what the config declares, and the
//!    remote retry policy would be cut off mid-attempt by an outer bound it knows
//!    nothing about.
//! 2. **Every listing is bounded** (`pagination`), on top of a bounded frame
//!    (`frame`). The second is what makes the first worth anything.
//! 3. **Nothing readable is stored.** Secrets are named by environment variable
//!    and read at connect time; the OAuth credential is resolved by the caller and
//!    handed in, so this crate never touches the OS secret store.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;
use tokio::time::Instant;

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::http::StreamableHttpAdapter;
use crate::pagination::collect_paginated;
use crate::stdio::{SHUTDOWN_GRACE, STDERR_DRAIN_GRACE, StderrTail};
use crate::text::{DESCRIPTION_CAP, INSTRUCTIONS_CAP, cap};

pub type CommandHardener = std::sync::Arc<dyn Fn(&mut Command) + Send + Sync>;

/// Delays between two handshake attempts on a remote server. Adapted from Codex
/// (`rmcp-client/src/streamable_http_retry.rs`): a load balancer answering 503
/// during a rollout is the common case, and one retry turns it into a non-event.
/// A delay is only taken when the startup budget still has room for it.
const HTTP_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

/// Live connection to an MCP server. Holds the `RunningService`: closing it
/// (`cancel`) or dropping it kills the subprocess.
pub struct McpConnection {
    service: RunningService<RoleClient, ()>,
    /// Bound of one tool call, resolved from the server config.
    tool_timeout: Option<Duration>,
    /// End of the startup budget, taken before the spawn. `tools/list` spends
    /// what the handshake left, so the whole step fits the declared bound.
    startup_deadline: Instant,
    /// `initialize.instructions`, capped. Untrusted, server-authored prose: it is
    /// shown on request (`/mcp <server> info`) and never reaches the model.
    instructions: Option<String>,
    /// Kept alive so a server that dies mid-session still has its last words.
    stderr: Option<StderrTail>,
    /// Subprocess handle of a stdio server. Dropping it kills the server
    /// (`kill_on_drop`), which is the lifecycle contract of this type. Boxed
    /// because `McpConnection` lives inside the `McpServer` enum, whose other
    /// variants hold nothing of the sort.
    child: Option<Box<tokio::process::Child>>,
}

/// Metadata of an exposed tool. The schemas stay attached here to allow
/// a future model exposure through a strict adapter, without redoing a handshake.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub original_name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    /// The MCP annotations are hints provided by the remote server. They must
    /// never become a security decision on the client side.
    pub annotations_untrusted: bool,
    /// `annotations.destructiveHint`. Read in ONE direction only: a `true`
    /// forces the confirmation back on, a `false` (or a missing value) grants
    /// nothing. A server may aggravate its own treatment, never soften it.
    pub destructive_hint: Option<bool>,
}

impl McpConnection {
    /// Spawns the stdio server and establishes the MCP handshake. `name` is used for the
    /// error label.
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_with(name, cfg, None, None).await
    }

    /// Full variant: the caller injects the same env scrub + proxy as the Bash
    /// tools (`harden`), and the bearer credential it resolved (`token`).
    ///
    /// Both injections are the caller's job on purpose. The hardener applies to
    /// the stdio transport alone, since a remote server is not a subprocess. The
    /// token is resolved outside because reading the OS secret store is an
    /// authentication concern, not a transport one: keeping it here would make a
    /// keyring failure look like a connection failure and hide it.
    pub async fn connect_with(
        name: &str,
        cfg: &McpServerConfig,
        harden: Option<&CommandHardener>,
        token: Option<&str>,
    ) -> Result<Self, McpError> {
        // Taken before anything is spawned or dialed: this is what makes
        // `startupTimeoutMs` cover the whole step rather than one of its parts.
        let deadline = Instant::now() + cfg.policy.startup_timeout();
        match &cfg.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                env_vars,
                cwd,
            } => {
                Self::connect_stdio(
                    name,
                    StdioSpawn {
                        program: command,
                        args,
                        env,
                        env_vars,
                        cwd: cwd.as_deref(),
                    },
                    cfg,
                    harden,
                    deadline,
                )
                .await
            }
            McpTransport::Http {
                url,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
                ..
            } => {
                // A configured environment variable always wins: it is the
                // explicit, auditable path. The credential the caller resolved is
                // the fallback, and its absence is not an error (the server may
                // accept anonymous calls and say so itself).
                let token = match bearer_token_env_var {
                    Some(var) => Some(read_env_secret(name, var, "bearer token")?),
                    None => token.map(str::to_string),
                };
                Self::connect_http(
                    name,
                    url,
                    token,
                    http_headers,
                    env_http_headers,
                    cfg,
                    deadline,
                )
                .await
            }
        }
    }

    /// Remote server over Streamable HTTP. Every secret is read from the
    /// environment HERE, by the name the config declares: no readable value ever
    /// enters `McpServerConfig`, hence neither the transcript nor the
    /// diagnostics (US-013 AC3).
    ///
    /// The handshake is retried on a transient failure (a 503 during a rollout,
    /// a stream closed before `initialized`), but only inside the startup budget:
    /// a retry policy that cannot finish within its own bound is a retry policy
    /// that never runs its later attempts.
    async fn connect_http(
        name: &str,
        url: &str,
        token: Option<String>,
        http_headers: &BTreeMap<String, String>,
        env_http_headers: &BTreeMap<String, String>,
        cfg: &McpServerConfig,
        deadline: Instant,
    ) -> Result<Self, McpError> {
        let headers = resolve_http_headers(name, http_headers, env_http_headers)?;
        let mut attempt = 0_usize;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(startup_expired(name, cfg));
            }
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
            if let Some(token) = token.clone() {
                config = config.auth_header(token);
            }
            config.custom_headers = headers.clone();
            let http = reqwest::Client::builder()
                .connect_timeout(remaining)
                // Idle connections are not reused: an SSE body that was not drained
                // stalls the next request on the same connection.
                .pool_max_idle_per_host(0)
                .build()
                .map_err(|e| McpError::Connect {
                    server: name.to_string(),
                    message: format!("http client: {e}"),
                })?;
            let transport = StreamableHttpClientTransport::with_client(
                StreamableHttpAdapter::new(http),
                config,
            );
            let message: String = match tokio::time::timeout(remaining, ().serve(transport)).await {
                Err(_) => return Err(startup_expired(name, cfg)),
                Ok(Err(e)) => e.to_string(),
                Ok(Ok(service)) => {
                    let instructions = instructions_of(&service);
                    return Ok(Self {
                        service,
                        tool_timeout: cfg.policy.tool_timeout,
                        startup_deadline: deadline,
                        instructions,
                        stderr: None,
                        child: None,
                    });
                }
            };
            // A delay is only worth taking when what follows it still fits.
            let left = deadline.saturating_duration_since(Instant::now());
            match HTTP_RETRY_DELAYS.get(attempt) {
                Some(delay) if is_retryable_connect_error(&message) && left > *delay => {
                    tokio::time::sleep(*delay).await;
                    attempt += 1;
                }
                _ => {
                    return Err(McpError::Connect {
                        server: name.to_string(),
                        message: with_login_hint(message, name),
                    });
                }
            }
        }
    }

    async fn connect_stdio(
        name: &str,
        spawn: StdioSpawn<'_>,
        cfg: &McpServerConfig,
        harden: Option<&CommandHardener>,
        deadline: Instant,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(spawn.program);
        command.args(spawn.args);
        if let Some(harden) = harden {
            harden(&mut command);
        }
        if let Some(cwd) = spawn.cwd {
            command.current_dir(cwd);
        }
        // Names first, literals second: an explicit `env` entry wins over a
        // forwarded variable of the same name.
        for key in spawn.env_vars {
            if is_proxy_env_key(key) {
                continue;
            }
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (k, v) in spawn.env {
            if is_proxy_env_key(k) {
                continue;
            }
            command.env(k, v);
        }
        let spawned = crate::stdio::spawn(&mut command).map_err(|e| McpError::Spawn {
            server: name.to_string(),
            source: e,
        })?;
        let tail = StderrTail::default();
        let reader = tail.reading(spawned.stderr);
        let mut child = spawned.child;
        let remaining = deadline.saturating_duration_since(Instant::now());
        // On expiry the `serve()` future is dropped in place; the child handle is
        // dropped with it and `kill_on_drop` collects the subprocess.
        let message: String = match tokio::time::timeout(
            remaining,
            ().serve((spawned.stdout, spawned.stdin)),
        )
        .await
        {
            Err(_) => format!("timeout after {}s", cfg.policy.startup_timeout().as_secs()),
            Ok(Err(e)) => e.to_string(),
            Ok(Ok(service)) => {
                let instructions = instructions_of(&service);
                return Ok(Self {
                    service,
                    tool_timeout: cfg.policy.tool_timeout,
                    startup_deadline: deadline,
                    instructions,
                    stderr: Some(tail),
                    child: Some(Box::new(child)),
                });
            }
        };
        // The process is gone, so its stderr is at EOF: a short grace is enough
        // for the reader to finish, and never blocks the failure path for long.
        if let Some(reader) = reader {
            let _ = tokio::time::timeout(STDERR_DRAIN_GRACE, reader).await;
        }
        let _ = child.start_kill();
        Err(McpError::Connect {
            server: name.to_string(),
            message: with_stderr(message, &tail.snapshot()),
        })
    }

    /// Cloneable call handle for this connection (US-010). Cloning the `Peer` does
    /// not clone the connection: the lifecycle stays owned by this
    /// `McpConnection`, and a call issued after `cancel` fails with a transport
    /// error rather than resurrecting a dead server.
    pub fn client(&self, server: &str) -> crate::call::McpClient {
        let client = crate::call::McpClient::new(server, self.service.peer().clone());
        match self.tool_timeout {
            Some(timeout) => client.with_timeout(timeout),
            None => client,
        }
    }

    /// `initialize.instructions`, capped. Untrusted, server-authored prose.
    ///
    /// Inspected on demand, never folded into a tool description: a description
    /// reaches the model as part of the tool definitions, which no tool output
    /// ever taints, so prose smuggled in there would be injection with the taint
    /// defense structurally unable to see it.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Last lines the server wrote on stderr, empty when there are none or when
    /// the server is remote.
    pub fn stderr_tail(&self) -> String {
        self.stderr
            .as_ref()
            .map(StderrTail::snapshot)
            .unwrap_or_default()
    }

    /// Lists the tools exposed by the server, on what the handshake left of the
    /// startup budget. Pagination is followed, bounded on every axis a server
    /// controls: pages, items, cursor size, cursor repetition.
    pub async fn list_tools(&self, name: &str) -> Result<Vec<McpToolInfo>, McpError> {
        let remaining = self
            .startup_deadline
            .saturating_duration_since(Instant::now());
        let listing = collect_paginated("tools", |params| async move {
            let page = self
                .service
                .list_tools(params)
                .await
                .map_err(|e| e.to_string())?;
            Ok((page.tools, page.next_cursor))
        });
        let tools = tokio::time::timeout(remaining, listing)
            .await
            .map_err(|_| McpError::Connect {
                server: name.to_string(),
                message: "list_tools: startup budget exhausted".to_string(),
            })?
            .map_err(|message| McpError::Connect {
                server: name.to_string(),
                message: with_stderr(format!("list_tools: {message}"), &self.stderr_tail()),
            })?;
        Ok(tools.into_iter().map(tool_info).collect())
    }

    /// Closes the connection cleanly: stdin closed by `cancel`, a bounded wait for
    /// the server to leave on its own, then the `kill_on_drop` of the handle.
    ///
    /// The `Result` of `cancel()` (a `JoinError` when the service task panicked)
    /// is deliberately ignored: the subprocess dies either way. Fire-and-forget.
    pub async fn cancel(self) {
        let _ = self.service.cancel().await;
        if let Some(mut child) = self.child {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
        }
    }
}

/// The stdio fields of one spawn, grouped so the spawn stays one argument rather
/// than five positional ones.
struct StdioSpawn<'a> {
    program: &'a str,
    args: &'a [String],
    env: &'a BTreeMap<String, String>,
    env_vars: &'a [String],
    cwd: Option<&'a std::path::Path>,
}

fn startup_expired(name: &str, cfg: &McpServerConfig) -> McpError {
    McpError::Connect {
        server: name.to_string(),
        message: format!("timeout after {}s", cfg.policy.startup_timeout().as_secs()),
    }
}

fn tool_info(t: rmcp::model::Tool) -> McpToolInfo {
    let destructive_hint = t
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.destructive_hint);
    McpToolInfo {
        name: t.name.to_string(),
        original_name: t.name.into_owned(),
        title: t.title,
        description: t
            .description
            .map(|d| cap(&d, DESCRIPTION_CAP))
            .unwrap_or_default(),
        input_schema: serde_json::Value::Object((*t.input_schema).clone()),
        output_schema: t
            .output_schema
            .map(|schema| serde_json::Value::Object((*schema).clone())),
        annotations_untrusted: t.annotations.is_some(),
        destructive_hint,
    }
}

fn instructions_of(service: &RunningService<RoleClient, ()>) -> Option<String> {
    service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .map(|text| cap(text.trim(), INSTRUCTIONS_CAP))
        .filter(|text| !text.is_empty())
}

/// Appends the server's last words to a failure message, bounded by the tail cap.
fn with_stderr(message: String, stderr: &str) -> String {
    if stderr.is_empty() {
        message
    } else {
        format!("{message}; stderr: {stderr}")
    }
}

/// Reads a secret by the NAME the config declares. The name travels in the error,
/// the value never does.
fn read_env_secret(server: &str, var: &str, what: &str) -> Result<String, McpError> {
    std::env::var(var)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| McpError::Connect {
            server: server.to_string(),
            message: format!("{what}: environment variable {var} is empty or unset"),
        })
}

/// Builds the custom header map: literal headers, then headers whose value is
/// read from the named environment variable.
fn resolve_http_headers(
    server: &str,
    http_headers: &BTreeMap<String, String>,
    env_http_headers: &BTreeMap<String, String>,
) -> Result<
    std::collections::HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    McpError,
> {
    let mut headers = std::collections::HashMap::new();
    let mut insert = |name: &str, value: String| -> Result<(), McpError> {
        let header_name =
            reqwest::header::HeaderName::try_from(name).map_err(|e| McpError::Connect {
                server: server.to_string(),
                message: format!("invalid header name \"{name}\": {e}"),
            })?;
        let mut header_value =
            reqwest::header::HeaderValue::try_from(value).map_err(|e| McpError::Connect {
                server: server.to_string(),
                // The VALUE is never echoed: an env-sourced header holds a secret.
                message: format!("invalid header value for \"{name}\": {e}"),
            })?;
        header_value.set_sensitive(true);
        headers.insert(header_name, header_value);
        Ok(())
    };
    for (name, value) in http_headers {
        insert(name, value.clone())?;
    }
    for (name, var) in env_http_headers {
        insert(name, read_env_secret(server, var, "http header")?)?;
    }
    Ok(headers)
}

/// Names the way out of an authentication failure. A bare `HTTP 401` leaves the
/// user guessing; the action to take is one command away.
fn with_login_hint(message: String, server: &str) -> String {
    if message.contains("HTTP 401") || message.contains("HTTP 403") {
        format!("{message} (authorization required: run `/mcp {server} login`)")
    } else {
        message
    }
}

/// Is this handshake failure worth another attempt? Transient statuses and a
/// stream that closed mid-handshake are; anything the server decided (auth,
/// content type, protocol) is not.
fn is_retryable_connect_error(message: &str) -> bool {
    const RETRYABLE_STATUS: [&str; 6] = [
        "HTTP 408", "HTTP 429", "HTTP 500", "HTTP 502", "HTTP 503", "HTTP 504",
    ];
    if RETRYABLE_STATUS
        .iter()
        .any(|status| message.contains(status))
    {
        return true;
    }
    let lowered = message.to_ascii_lowercase();
    ["channel closed", "connection closed", "connection reset"]
        .iter()
        .any(|needle| lowered.contains(needle))
}

fn is_proxy_env_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_env_keys_are_filtered_case_insensitively() {
        assert!(is_proxy_env_key("HTTP_PROXY"));
        assert!(is_proxy_env_key("https_proxy"));
        assert!(is_proxy_env_key("All_Proxy"));
        assert!(is_proxy_env_key("NO_PROXY"));
        assert!(!is_proxy_env_key("PATH"));
        assert!(!is_proxy_env_key("API_TOKEN"));
    }

    #[test]
    fn a_failure_message_carries_the_server_last_words() {
        assert_eq!(
            with_stderr("timeout after 10s".to_string(), "Error: missing API key"),
            "timeout after 10s; stderr: Error: missing API key"
        );
        assert_eq!(with_stderr("boom".to_string(), ""), "boom");
    }

    #[test]
    fn only_transient_handshake_failures_are_retried() {
        assert!(is_retryable_connect_error("HTTP 503: upstream not ready"));
        assert!(is_retryable_connect_error("HTTP 429"));
        assert!(is_retryable_connect_error(
            "transport error: channel closed"
        ));
        // Decided by the server: retrying would only repeat the refusal.
        assert!(!is_retryable_connect_error("HTTP 401: unauthorized"));
        assert!(!is_retryable_connect_error("HTTP 404"));
        assert!(!is_retryable_connect_error("unexpected content type"));
    }

    #[test]
    fn a_missing_secret_names_the_variable_and_not_its_value() {
        let err = read_env_secret("srv", "PYXIS_TEST_ABSENT_VAR", "bearer token")
            .expect_err("unset variable");
        let message = err.to_string();
        assert!(message.contains("PYXIS_TEST_ABSENT_VAR"), "{message}");
        assert!(message.contains("bearer token"), "{message}");
    }

    #[test]
    fn literal_headers_are_resolved_and_marked_sensitive() {
        let headers = resolve_http_headers(
            "srv",
            &BTreeMap::from([("X-Api-Version".to_string(), "2".to_string())]),
            &BTreeMap::new(),
        )
        .expect("valid headers");
        let value = headers
            .get(&reqwest::header::HeaderName::from_static("x-api-version"))
            .expect("header present");
        assert_eq!(value.to_str().ok(), Some("2"));
        // Marked sensitive so a `Debug` of the request does not print it.
        assert!(value.is_sensitive());
    }

    #[test]
    fn an_invalid_header_name_is_refused_by_name_only() {
        let err = resolve_http_headers(
            "srv",
            &BTreeMap::from([("bad header".to_string(), "v".to_string())]),
            &BTreeMap::new(),
        )
        .expect_err("invalid name");
        assert!(err.to_string().contains("bad header"));
    }
}
