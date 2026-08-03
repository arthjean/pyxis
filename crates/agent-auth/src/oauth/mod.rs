//! OAuth flows. `openai_chatgpt` = ChatGPT subscription (ADR-10); `mcp` = per
//! server authorization for remote MCP endpoints (MCP spec 2025-06-18).
//!
//! What the flows share lives here rather than in one of them: the error type
//! (`error`), the loopback redirect they both come back through (`callback`),
//! the PKCE helper (`pkce`), and the clock every expiry in this crate is
//! expressed in.

mod callback;
mod error;

pub mod mcp;
pub mod openai_chatgpt;
pub mod pkce;

pub use error::AuthError;

use rand::RngCore;

/// Milliseconds since the Unix epoch. Every `expires_at` in this crate is
/// absolute and expressed in this clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh anti-CSRF `state` for one authorization request.
pub(crate) fn random_state() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
