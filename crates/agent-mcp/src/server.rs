//! Runtime state of the MCP servers: discriminated enum (the client is only accessible
//! in `Connected`, guaranteed at compile time) + registry indexed by name.

use std::collections::BTreeMap;

use crate::client::{McpConnection, McpToolInfo};
use crate::config::{McpConfigFile, McpConfigIssue, McpServerConfig};
use crate::error::McpError;

/// State of an MCP server. The `conn` only exists in `Connected`: it is impossible
/// to call a server that is not connected.
pub enum McpServer {
    Disconnected {
        config: McpServerConfig,
    },
    Connecting {
        config: McpServerConfig,
    },
    Connected {
        config: McpServerConfig,
        conn: McpConnection,
        tools: Vec<McpToolInfo>,
    },
    Failed {
        config: McpServerConfig,
        error: String,
    },
}

impl McpServer {
    pub fn config(&self) -> &McpServerConfig {
        match self {
            McpServer::Disconnected { config }
            | McpServer::Connecting { config }
            | McpServer::Connected { config, .. }
            | McpServer::Failed { config, .. } => config,
        }
    }

    /// Number of exposed tools (0 outside `Connected`).
    pub fn tool_count(&self) -> usize {
        match self {
            McpServer::Connected { tools, .. } => tools.len(),
            _ => 0,
        }
    }

    /// Exposed tools (empty outside `Connected`).
    pub fn tools(&self) -> &[McpToolInfo] {
        match self {
            McpServer::Connected { tools, .. } => tools,
            _ => &[],
        }
    }
}

/// Registry of the known MCP servers, indexed by name (stable lexicographic order).
/// The state transitions are synchronous; the network connection itself happens
/// outside the registry (the caller releases the lock before the `await`).
#[derive(Default)]
pub struct McpRegistry {
    servers: BTreeMap<String, McpServer>,
    issues: Vec<McpConfigIssue>,
}

impl McpRegistry {
    /// Builds the registry from the config: every server `Disconnected`.
    pub fn from_config(file: McpConfigFile) -> Self {
        let servers = file
            .servers
            .into_iter()
            .map(|(name, config)| (name, McpServer::Disconnected { config }))
            .collect();
        Self {
            servers,
            issues: file.issues,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &McpServer)> {
        self.servers.iter()
    }

    pub fn issues(&self) -> &[McpConfigIssue] {
        &self.issues
    }

    pub fn issue_count(&self) -> usize {
        self.issues.len()
    }

    /// Moves a server to `Connecting`; returns its config (to spawn) and
    /// the previous connection when there is one (reconnect case: to be closed by the caller).
    pub fn begin_connect(
        &mut self,
        name: &str,
    ) -> Result<(McpServerConfig, Option<McpConnection>), McpError> {
        let server = self
            .servers
            .get_mut(name)
            .ok_or_else(|| McpError::Unknown(name.to_string()))?;
        // Already connecting -> we refuse (avoids a second process spawn).
        if matches!(server, McpServer::Connecting { .. }) {
            return Err(McpError::Connect {
                server: name.to_string(),
                message: "connection already in progress".to_string(),
            });
        }
        let config = server.config().clone();
        let prev = std::mem::replace(
            server,
            McpServer::Connecting {
                config: config.clone(),
            },
        );
        let old_conn = match prev {
            McpServer::Connected { conn, .. } => Some(conn),
            _ => None,
        };
        Ok((config, old_conn))
    }

    /// Moves a server back to `Disconnected`; returns the connection to close (when there is one).
    pub fn begin_disconnect(&mut self, name: &str) -> Option<McpConnection> {
        let server = self.servers.get_mut(name)?;
        let config = server.config().clone();
        match std::mem::replace(server, McpServer::Disconnected { config }) {
            McpServer::Connected { conn, .. } => Some(conn),
            _ => None,
        }
    }

    /// Applies the success of a connection. Only applies when the server is
    /// still `Connecting` (otherwise the user disconnected in the meantime: the
    /// connection is returned to the caller for closing).
    #[must_use = "the returned connection must be closed (cancel)"]
    pub fn finish_connect(
        &mut self,
        name: &str,
        conn: McpConnection,
        tools: Vec<McpToolInfo>,
    ) -> Option<McpConnection> {
        match self.servers.get_mut(name) {
            Some(server @ McpServer::Connecting { .. }) => {
                let config = server.config().clone();
                *server = McpServer::Connected {
                    config,
                    conn,
                    tools,
                };
                None
            }
            _ => Some(conn),
        }
    }

    /// Marks a server `Failed` (spawn or handshake failed). No effect when the
    /// server is no longer `Connecting`.
    pub fn fail(&mut self, name: &str, error: String) {
        if let Some(server @ McpServer::Connecting { .. }) = self.servers.get_mut(name) {
            let config = server.config().clone();
            *server = McpServer::Failed { config, error };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpConfigFile;
    use std::collections::BTreeMap;

    fn registry_with(name: &str) -> McpRegistry {
        let mut servers = BTreeMap::new();
        servers.insert(name.to_string(), McpServerConfig::stdio("echo", Vec::new()));
        McpRegistry::from_config(McpConfigFile {
            servers,
            skipped: 0,
            issues: Vec::new(),
        })
    }

    #[test]
    fn begin_connect_rejects_when_already_connecting() {
        let mut reg = registry_with("srv");
        // 1st begin_connect: Disconnected -> Connecting, no previous connection.
        let (_cfg, old) = reg.begin_connect("srv").unwrap();
        assert!(old.is_none());
        // 2nd one during Connecting -> refused (avoids a double process spawn).
        assert!(reg.begin_connect("srv").is_err());
    }

    #[test]
    fn begin_connect_unknown_server_errs() {
        let mut reg = registry_with("srv");
        assert!(reg.begin_connect("absent").is_err());
    }

    #[test]
    fn begin_disconnect_resets_and_unblocks_connect() {
        let mut reg = registry_with("srv");
        reg.begin_connect("srv").unwrap();
        // In Connecting (not Connected) -> no connection to close.
        assert!(reg.begin_disconnect("srv").is_none());
        // Back to Disconnected -> begin_connect works again.
        assert!(reg.begin_connect("srv").is_ok());
    }
}
