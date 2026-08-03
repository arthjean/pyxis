//! `agent-app-server`: the external client contract (EP-005).
//!
//! A JSON-RPC 2.0 surface over the durable thread runtime, on the Codex
//! app-server concepts: a connection initializes, opens or resumes a thread,
//! starts and interrupts turns, answers approvals, walks the history and reads
//! an ordered item stream under bounded back-pressure.
//!
//! Boundaries:
//! - The runtime assembly (provider, tools, sandbox, sessions) stays in the
//!   binary and enters through [`host::RuntimeHost`]. Nothing here opens a
//!   file, resolves a model or decides a permission.
//! - Permissions, taint, hooks and cancellation are NOT re-implemented: an
//!   approval crosses [`bridge::ClientBridge`] into the existing pipeline, and
//!   a client-declared tool is an ordinary registry tool.
//! - Every published name lives in one of the three tables of [`protocol`], so
//!   the dispatcher, the capabilities and the generated schemas cannot disagree.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
mod cursor;
pub mod host;
pub mod items;
pub mod jsonrpc;
pub mod outbound;
pub mod protocol;
mod protocol_metadata;
mod pump;
pub mod schema;
pub mod server;
pub mod transport;

pub use bridge::{BridgeApprover, ClientBridge, ClientEndpoint, RejectedTool, dynamic_tools};
pub use host::{HostError, OpenThread, RuntimeHost, ThreadControl};
pub use outbound::{MAX_QUEUED_BYTES, MAX_QUEUED_EVENTS};
pub use protocol::{
    CLIENT_METHODS, DynamicToolSpec, PROTOCOL_VERSION, SERVER_NOTIFICATIONS, SERVER_REQUESTS,
    ThreadItem,
};
pub use server::{AppServer, Connection};
pub use transport::{ServeError, mint_token, serve_stdio, serve_websocket};
