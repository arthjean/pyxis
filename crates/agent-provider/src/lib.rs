//! `agent-provider` — adapters implémentant le trait `Provider` du cœur
//! (`agent-core`). Cible MVP : `OpenAiChatGpt` — abonnement ChatGPT via la
//! Responses API sur le backend ChatGPT/Codex (ADR-10), SSE stateless.
//!
//! Le canonique (Anthropic-like, transcript client-side) et le vocabulaire
//! `StreamEvent` vivent dans `agent-core` (invariant 1 : le cœur ne dépend pas
//! des adapters ; il consomme `dyn Provider`). Réseau maison : `reqwest` +
//! `eventsource-stream`, sans SDK (PROVIDERS §1.1).
//!
//! Les autres providers (Anthropic, OpenAI Chat BYOK, Gemini…) s'ajouteront
//! ensuite, chacun comme un module ici. Ollama est hors scope (retiré).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod chatgpt;
pub mod chatgpt_events;
pub mod chatgpt_request;
pub mod credential;

pub use chatgpt::{
    DEFAULT_MAX_CONTEXT, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT, KEYRING_ACCOUNT,
    OpenAiChatGptProvider,
};
pub use chatgpt_events::CodexEventMapper;
pub use chatgpt_request::build_responses_body;
pub use credential::CredentialManager;
