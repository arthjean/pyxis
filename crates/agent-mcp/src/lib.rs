//! `agent-mcp`: MCP (Model Context Protocol) integration through the official
//! `rmcp` SDK (ARCHITECTURE 6). State of a server as a discriminated enum: the client
//! is only accessible in `Connected`.
//!
//! Current scope (early Phase 2): **stdio** transport, connect / disconnect /
//! reconnect life cycle, tool listing (capped descriptions). Deferred:
//! wrapping the tools into `DynTool` (`agent-tools` registry), per-server
//! PKCE OAuth and the SSE / HTTP transports.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod client;
mod config;
mod error;
mod server;

pub use client::{CommandHardener, McpConnection, McpToolInfo};
pub use config::{
    McpConfigFile, McpConfigIssue, McpConfigIssueKind, McpConfigOrigin, McpConfigSource,
    McpServerConfig,
};
pub use error::McpError;
pub use server::{McpRegistry, McpServer};
