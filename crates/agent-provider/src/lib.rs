//! `agent-provider`: adapters implementing the core `Provider` trait
//! (`agent-core`). MVP target: `OpenAiChatGpt`, the ChatGPT subscription through the
//! Responses API on the ChatGPT/Codex backend (ADR-10), with WebSocket and
//! deterministic HTTP/SSE fallback.
//!
//! The canonical model (Anthropic-like, client-side transcript) and the
//! `StreamEvent` vocabulary live in `agent-core` (invariant 1: the core does not depend
//! on the adapters; it consumes `dyn Provider`). In-house networking: `reqwest`,
//! `eventsource-stream`, and `tokio-tungstenite`, without an SDK (PROVIDERS 1.1).
//!
//! The other providers (Anthropic, OpenAI Chat BYOK, Gemini, ...) will be added
//! later, each as a module here. Ollama is out of scope (dropped).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod chatgpt;
mod chatgpt_error;
pub mod chatgpt_events;
pub mod chatgpt_http;
mod chatgpt_metadata;
pub mod chatgpt_request;
mod chatgpt_websocket;
pub mod credential;
pub mod models;
pub mod openai;
pub mod quota;
mod responses;

pub use chatgpt::{
    DEFAULT_MAX_CONTEXT, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT, KEYRING_ACCOUNT,
    OpenAiChatGptProvider,
};
pub use chatgpt_events::CodexEventMapper;
pub use chatgpt_http::{ResponsesCompression, ResponsesTransportConfig};
pub use chatgpt_request::build_responses_body;
pub use chatgpt_websocket::{
    WebSocketProbeAuthorization, WebSocketProbeReport, WebSocketProbeVerdict,
};
pub use credential::CredentialManager;
pub use models::CatalogModel;
pub use openai::{
    AuthRecovery, ConfiguredOpenAiConfig, ConfiguredOpenAiProvider, OpenAiAccountState,
    OpenAiAuthPolicy, OpenAiCatalogPolicy, OpenAiEndpointKind,
};
pub use quota::{parse_all_quota_headers, parse_quota_headers, quota_refusal_message};
