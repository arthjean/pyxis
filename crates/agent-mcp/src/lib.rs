//! `agent-mcp`: MCP (Model Context Protocol) integration through the official
//! `rmcp` SDK (ARCHITECTURE 6). State of a server as a discriminated enum: the client
//! is only accessible in `Connected`.
//!
//! Current scope: **stdio** and **Streamable HTTP** transports (EP-004), connect /
//! disconnect / reconnect life cycle, bounded tool listing, a per-server exposure
//! filter, approval level and call bounds, secrets named by environment variable
//! on both transports, per-server OAuth whose credential is resolved by the
//! caller, resources and server instructions offered to inspection only, images
//! carried back to the model, and tool calling exposed to the model loop as
//! `DynTool`. Deferred: the superseded SSE transport, MCP elicitation and
//! progress notifications.
//!
//! Bounds live where the untrusted bytes arrive, one module each: `frame` (one
//! JSON-RPC message, under every other bound), `pagination` (one listing),
//! `text` (server-authored prose). Everything above them may then assume a
//! message that fits in memory.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod call;
mod client;
mod config;
mod error;
mod frame;
mod http;
mod naming;
mod pagination;
mod resources;
mod schema;
mod server;
mod stdio;
mod text;
mod tool;

pub use call::{DEFAULT_CALL_TIMEOUT, McpCallOutcome, McpClient};
pub use client::{CommandHardener, McpConnection, McpToolInfo};
pub use config::{
    DEFAULT_STARTUP_TIMEOUT, McpApproval, McpConfigFile, McpConfigIssue, McpConfigIssueKind,
    McpConfigOrigin, McpConfigSource, McpServerConfig, McpServerPolicy, McpToolPolicy,
    McpTransport,
};
pub use error::McpError;
pub use naming::{MAX_NAME_BYTES, NAME_PREFIX, qualified_name};
pub use resources::McpResourceInfo;
pub use schema::{MAX_SCHEMA_BYTES, strict_input_schema};
pub use server::{McpRegistry, McpServer};
pub use text::INSTRUCTIONS_CAP;
pub use tool::{McpTool, McpToolPlan, McpToolSkipped, dyn_tools, filter_tools, plan_tools};
