//! `agent-mcp` — intégration MCP (Model Context Protocol) via le SDK officiel
//! `rmcp` (ARCHITECTURE §6). État d'un serveur en enum discriminé : le client
//! n'est accessible que dans `Connected`.
//!
//! Périmètre actuel (en tête de Phase 2) : transport **stdio**, cycle de vie
//! connect / disconnect / reconnect, liste des outils (descriptions cappées). Sont
//! reportés : le wrapping des outils en `DynTool` (registre `agent-tools`), l'OAuth
//! PKCE par serveur et les transports SSE / HTTP.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod client;
mod config;
mod error;
mod server;

pub use client::{CommandHardener, McpConnection, McpToolInfo};
pub use config::{McpConfigFile, McpServerConfig};
pub use error::McpError;
pub use server::{McpRegistry, McpServer};
