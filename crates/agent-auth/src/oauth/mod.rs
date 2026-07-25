//! OAuth subscription flows. `openai_chatgpt` = ChatGPT subscription (ADR-10);
//! `pkce` = shared RFC 7636 helper (Anthropic OAuth will reuse it).

pub mod openai_chatgpt;
pub mod pkce;
