//! MCP client: connection to a server through the stdio transport (subprocess) or
//! the Streamable HTTP transport (remote, US-013), automatic `initialize`
//! handshake, tool listing. Both transports produce the same `McpConnection`: the
//! rest of the crate never learns which one it is talking to.

use std::sync::Arc;
use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::http::StreamableHttpAdapter;

pub type CommandHardener = Arc<dyn Fn(&mut Command) + Send + Sync>;

/// Max delay to establish the connection (spawn + `initialize` handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Same bound for a remote server, but tighter: a network handshake that has not
/// completed in 10s will not complete (NFR of the PRD). Applies to the TCP/TLS
/// connection and to the whole `initialize`.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Length cap of a tool description (ARCHITECTURE 6: a server cannot
/// pollute the prompt).
const DESCRIPTION_CAP: usize = 2048;

/// Live connection to a stdio MCP server. Holds the `RunningService`: closing it
/// (`cancel`) or dropping it kills the subprocess.
pub struct McpConnection {
    service: RunningService<RoleClient, ()>,
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
}

impl McpConnection {
    /// Spawns the stdio server and establishes the MCP handshake. `name` is used for the
    /// error label.
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_hardened(name, cfg, None).await
    }

    /// Hardened variant: the caller can inject the same env scrub + proxy as the
    /// Bash tools. `cfg.env` stays explicit, but the proxy keys are ignored
    /// to avoid bypasses through `NO_PROXY` or `ALL_PROXY`.
    ///
    /// The hardener applies to the stdio transport alone: a remote server is not a
    /// subprocess, so there is no command to harden.
    pub async fn connect_hardened(
        name: &str,
        cfg: &McpServerConfig,
        harden: Option<&CommandHardener>,
    ) -> Result<Self, McpError> {
        match &cfg.transport {
            McpTransport::Stdio { command, args, env } => {
                Self::connect_stdio(name, command, args, env, harden).await
            }
            McpTransport::Http {
                url,
                bearer_token_env_var,
            } => Self::connect_http(name, url, bearer_token_env_var.as_deref()).await,
        }
    }

    /// Remote server over Streamable HTTP. The bearer token is read from the
    /// environment HERE, by the name the config declares: no readable secret ever
    /// enters `McpServerConfig`, hence neither the transcript nor the diagnostics
    /// (US-013 AC3).
    async fn connect_http(
        name: &str,
        url: &str,
        bearer_token_env_var: Option<&str>,
    ) -> Result<Self, McpError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        if let Some(var) = bearer_token_env_var {
            let token = std::env::var(var)
                .ok()
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| McpError::Connect {
                    server: name.to_string(),
                    // The NAME of the variable, never its content.
                    message: format!("bearer token: environment variable {var} is empty or unset"),
                })?;
            config = config.auth_header(token);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            // Idle connections are not reused: an SSE body that was not drained
            // stalls the next request on the same connection.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| McpError::Connect {
                server: name.to_string(),
                message: format!("http client: {e}"),
            })?;
        let transport =
            StreamableHttpClientTransport::with_client(StreamableHttpAdapter::new(http), config);
        let service = tokio::time::timeout(HTTP_CONNECT_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| McpError::Connect {
                server: name.to_string(),
                message: format!("timeout after {}s", HTTP_CONNECT_TIMEOUT.as_secs()),
            })?
            .map_err(|e| McpError::Connect {
                server: name.to_string(),
                message: e.to_string(),
            })?;
        Ok(Self { service })
    }

    async fn connect_stdio(
        name: &str,
        program: &str,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        harden: Option<&CommandHardener>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(harden) = harden {
            harden(&mut command);
        }
        for (k, v) in env {
            if is_proxy_env_key(k) {
                continue;
            }
            command.env(k, v);
        }
        // Server stderr is discarded on purpose: `rmcp` inherits it by default, and
        // since the servers are now spawned at startup (US-012) a chatty server
        // would write straight over the TUI, which owns the terminal. A handshake
        // failure is already reported through `McpError::Connect`.
        let (transport, _no_stderr) = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| McpError::Spawn {
                server: name.to_string(),
                source: e,
            })?;
        // On timeout, the `serve()` future is dropped in place and the subprocess is
        // killed through the `Drop` of the transport (detached kill). Enough for a
        // long-running CLI; an explicit graceful shutdown (serve_with_ct) stays possible.
        let service: RunningService<RoleClient, ()> =
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| McpError::Connect {
                    server: name.to_string(),
                    message: format!("timeout after {}s", CONNECT_TIMEOUT.as_secs()),
                })?
                .map_err(|e| McpError::Connect {
                    server: name.to_string(),
                    message: e.to_string(),
                })?;
        Ok(Self { service })
    }

    /// Cloneable call handle for this connection (US-010). Cloning the `Peer` does
    /// not clone the connection: the lifecycle stays owned by this
    /// `McpConnection`, and a call issued after `cancel` fails with a transport
    /// error rather than resurrecting a dead server.
    pub fn client(&self, server: &str) -> crate::call::McpClient {
        crate::call::McpClient::new(server, self.service.peer().clone())
    }

    /// Lists the tools exposed by the server (descriptions capped at 2048 chars).
    pub async fn list_tools(&self, name: &str) -> Result<Vec<McpToolInfo>, McpError> {
        let tools = tokio::time::timeout(LIST_TOOLS_TIMEOUT, self.service.list_all_tools())
            .await
            .map_err(|_| McpError::Connect {
                server: name.to_string(),
                message: format!("list_tools timeout after {}s", LIST_TOOLS_TIMEOUT.as_secs()),
            })?
            .map_err(|e| McpError::Connect {
                server: name.to_string(),
                message: format!("list_tools: {e}"),
            })?;
        Ok(tools
            .into_iter()
            .map(|t| McpToolInfo {
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
            })
            .collect())
    }

    /// Closes the connection cleanly (stdin closed, bounded wait, then kill).
    ///
    /// The `Result` of `cancel()` (a `JoinError` when the service task panicked)
    /// is deliberately ignored: the subprocess is killed anyway by the
    /// `Drop` of the transport. Called fire-and-forget.
    pub async fn cancel(self) {
        let _ = self.service.cancel().await;
    }
}

/// Truncates `s` to `max` chars (never in the middle of a multi-byte char).
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
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
}
