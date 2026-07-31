//! OAuth flows. `openai_chatgpt` = ChatGPT subscription (ADR-10); `mcp` = per
//! server authorization for remote MCP endpoints (MCP spec 2025-06-18);
//! `pkce` = shared RFC 7636 helper (Anthropic OAuth will reuse it).

pub mod mcp;
pub mod openai_chatgpt;
pub mod pkce;
