//! Errors of the MCP layer: configuration, startup and connection of a server.

use std::path::PathBuf;

/// Error while loading the config or connecting to an MCP server. The message
/// (Display) inlines the cause, which is enough for the TUI display.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("lecture de {0} : {1}")]
    Read(PathBuf, std::io::Error),
    #[error("JSON invalide dans {0} : {1}")]
    Parse(PathBuf, serde_json::Error),
    #[error("server \"{server}\": failed to start process: {source}")]
    Spawn {
        server: String,
        source: std::io::Error,
    },
    #[error("serveur « {server} » : {message}")]
    Connect { server: String, message: String },
    /// Transport or protocol failure of a tool call. A *functional* failure of the
    /// tool never lands here: it comes back as a tool result in error, per the
    /// separation the MCP protocol imposes.
    #[error("serveur MCP « {server} », outil « {tool} » : {message}")]
    Call {
        server: String,
        tool: String,
        message: String,
    },
    #[error("serveur MCP « {0} » inconnu")]
    Unknown(String),
}
