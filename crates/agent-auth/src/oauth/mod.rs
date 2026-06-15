//! Flows OAuth subscription. `openai_chatgpt` = abonnement ChatGPT (ADR-10) ;
//! `pkce` = helper RFC 7636 partagé (Anthropic OAuth le réutilisera).

pub mod openai_chatgpt;
pub mod pkce;
