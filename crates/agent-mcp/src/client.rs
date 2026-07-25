//! MCP client: connection to a server through the stdio transport (`rmcp`), automatic
//! `initialize` handshake, tool listing. Wrapping the tools into `DynTool`
//! (integration into the `agent-tools` registry) will come in Phase 2.

use std::sync::Arc;
use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::config::McpServerConfig;
use crate::error::McpError;

pub type CommandHardener = Arc<dyn Fn(&mut Command) + Send + Sync>;

/// Max delay to establish the connection (spawn + `initialize` handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
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
    pub async fn connect_hardened(
        name: &str,
        cfg: &McpServerConfig,
        harden: Option<&CommandHardener>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(&cfg.command);
        command.args(&cfg.args);
        if let Some(harden) = harden {
            harden(&mut command);
        }
        for (k, v) in &cfg.env {
            if is_proxy_env_key(k) {
                continue;
            }
            command.env(k, v);
        }
        let transport = TokioChildProcess::new(command).map_err(|e| McpError::Spawn {
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
