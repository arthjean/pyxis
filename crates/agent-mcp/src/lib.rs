//! `agent-mcp`: MCP (Model Context Protocol) integration through the official
//! `rmcp` SDK (ARCHITECTURE 6). State of a server as a discriminated enum: the client
//! is only accessible in `Connected`.
//!
//! Current scope: **stdio** transport, connect / disconnect / reconnect life
//! cycle, tool listing (capped descriptions), and tool calling exposed to the
//! model loop as `DynTool` (EP-003). Deferred: per-server PKCE OAuth, the SSE /
//! HTTP transports, and MCP resources / elicitation / progress notifications.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod call;
mod client;
mod config;
mod error;
mod server;
mod tool;

pub use call::{DEFAULT_CALL_TIMEOUT, McpCallOutcome, McpClient};
pub use client::{CommandHardener, McpConnection, McpToolInfo};
pub use config::{
    McpConfigFile, McpConfigIssue, McpConfigIssueKind, McpConfigOrigin, McpConfigSource,
    McpServerConfig,
};
pub use error::McpError;
pub use server::{McpRegistry, McpServer};
pub use tool::{
    MAX_NAME_BYTES, MAX_SCHEMA_BYTES, McpTool, McpToolPlan, McpToolSkipped, NAME_PREFIX, dyn_tools,
    plan_tools, qualified_name, strict_input_schema,
};
