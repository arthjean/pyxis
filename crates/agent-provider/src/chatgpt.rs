//! Adapter `OpenAiChatGpt` — abonnement ChatGPT via la Responses API sur le
//! backend ChatGPT/Codex (ADR-10). Implémente `agent_core::Provider`.
//!
//! **SSE stateless** : `server_side_state = false` → pas de `previous_response_id`,
//! contexte complet reconstruit côté client à chaque tour → mappe proprement le
//! canonique (PROVIDERS §4.1, le piège WebSocket+state est explicitement évité).
//!
//! ⚠️ Risque #1 à valider au premier run live (non testable ici, pas de token) :
//! le backend est un modèle à raisonnement et reçoit `include:
//! ["reasoning.encrypted_content"]`. Le MVP **n'réinjecte pas** les reasoning
//! items aux tours suivants. Si le backend rejette (`400` « reasoning item
//! required ») un `function_call` non précédé de son reasoning item, le fix est
//! borné : porter le `thinkingSignature` de Pi (capturer l'item reasoning à
//! `output_item.done`, le stocker dans le transcript, le réémettre dans
//! `input[]`). Voir `docs/openai-subscription-auth.md` §1.b.

use agent_auth::OAuthCredential;
use agent_core::message::ContentBlock;
use agent_core::provider::{
    AuthError, CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider,
    ProviderError, ProviderKind, StopReason, StreamEvent, TokenUsage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_request::build_responses_body;
use crate::credential::CredentialManager;

/// Clé keyring de la credential abonnement ChatGPT (refresh rotatif réécrit ici).
pub const KEYRING_ACCOUNT: &str = "oauth:openai_chatgpt";

/// Fenêtre de contexte par défaut (modèles GPT-5.x du backend Codex). **Valeur
/// volatile/à ajuster** : n'affecte QUE les seuils de compaction ; un dépassement
/// réel déclenche la compaction réactive (413, withholding). Conservatrice.
pub const DEFAULT_MAX_CONTEXT: u32 = 256_000;

/// Effort de raisonnement par défaut (Codex CLI ≈ "medium").
pub const DEFAULT_REASONING_EFFORT: &str = "medium";

/// Slug de modèle par défaut. Le backend Codex (abonnement ChatGPT) impose une
/// liste blanche de slugs VERSIONNÉS qu'il fait évoluer (retraits fréquents) : le
/// slug générique `gpt-5` est rejeté en 400 ("not supported when using Codex with
/// a ChatGPT account"). **Valeur volatile** — surchargeable via `--model` ou la
/// commande `/models` en session (voir `agent_tui::MODELS`).
pub const DEFAULT_MODEL: &str = "gpt-5.5";

/// Borne du corps d'erreur HTTP capturé (évite un message géant en log).
const MAX_ERR_BODY: usize = 2000;

pub struct OpenAiChatGptProvider {
    creds: CredentialManager,
    http: reqwest::Client,
    capabilities: Capabilities,
    reasoning_effort: Option<String>,
}

impl OpenAiChatGptProvider {
    /// Construit l'adapter depuis une credential OAuth déjà chargée (par la CLI,
    /// depuis le keyring). `max_context` pilote la compaction ; `reasoning_effort`
    /// = `None` omet le champ `reasoning`.
    pub fn new(cred: OAuthCredential, max_context: u32, reasoning_effort: Option<String>) -> Self {
        let http = reqwest::Client::new();
        let creds = CredentialManager::new(cred, http.clone(), KEYRING_ACCOUNT);
        Self {
            creds,
            http,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                // caching implicite côté backend, non contrôlé explicitement.
                prompt_caching: false,
                reasoning: true,
                // CLÉ : SSE stateless → le canonique client-side mappe (PROVIDERS §4.1).
                server_side_state: false,
                max_context,
            },
            reasoning_effort,
        }
    }

    /// Constructeur de confort : défauts MVP (`DEFAULT_MAX_CONTEXT`, effort medium).
    pub fn from_credential(cred: OAuthCredential) -> Self {
        Self::new(
            cred,
            DEFAULT_MAX_CONTEXT,
            Some(DEFAULT_REASONING_EFFORT.to_string()),
        )
    }
}

#[async_trait]
impl Provider for OpenAiChatGptProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        // 1. credential fraîche (refresh + keyring si besoin) → URL + en-têtes.
        let spec = self.creds.request_spec().await?;
        // 2. corps Responses (SSE stateless).
        let body = build_responses_body(&req, self.reasoning_effort.as_deref());

        // 3. POST. `.json()` pose content-type ; on ajoute les en-têtes propriétaires
        //    (Authorization, chatgpt-account-id, originator, OpenAI-Beta, accept).
        let mut rb = self.http.post(&spec.url).json(&body);
        for (k, v) in &spec.headers {
            if !k.eq_ignore_ascii_case("content-type") {
                rb = rb.header(k, v);
            }
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        // 4. statut. 413 → erreur de contexte (withholding/compaction réactive).
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            if code == 413 {
                return Err(ProviderError::ContextLengthExceeded);
            }
            let mut text = resp.text().await.unwrap_or_default();
            text.truncate(MAX_ERR_BODY);
            return Err(ProviderError::Http {
                status: code,
                message: text,
            });
        }

        // 5. flux SSE → StreamEvent canoniques (jamais d'ANSI, jamais de panic).
        let mut es = resp.bytes_stream().eventsource();
        let mut mapper = CodexEventMapper::new();
        let s = async_stream::stream! {
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(event) => match mapper.ingest(&event.data) {
                        Ok(events) => {
                            for e in events {
                                yield Ok(e);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    },
                    Err(e) => {
                        yield Err(ProviderError::Stream(e.to_string()));
                        return;
                    }
                }
            }
        };
        Ok(s.boxed())
    }

    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        // Réutilise le chemin stream et agrège (titres / résumés de compaction).
        let stream = self.stream(req).await?;
        futures_util::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let mut stop = StopReason::EndTurn;
        while let Some(ev) = stream.next().await {
            match ev? {
                StreamEvent::TextDelta { text: t } => text.push_str(&t),
                StreamEvent::Usage { usage: u } => usage = u,
                StreamEvent::Done { stop: s } => stop = s,
                _ => {}
            }
        }
        Ok(CanonicalResponse {
            content: vec![ContentBlock::Text { text }],
            usage,
            stop,
        })
    }

    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        match err {
            ProviderError::Http { status, .. } => match *status {
                401 | 403 => ErrorClass::Auth(AuthError::Invalid),
                429 => ErrorClass::RateLimited,
                529 => ErrorClass::Overloaded(529),
                s if s >= 500 => ErrorClass::Retryable,
                _ => ErrorClass::InvalidRequest,
            },
            // Transitoires : transport, flux coupé, chunk garbled → retry transverse.
            ProviderError::Transport(_) | ProviderError::Stream(_) | ProviderError::Decode(_) => {
                ErrorClass::Retryable
            }
            // N'atteint pas classify (is_context_error géré en amont) ; fail-safe.
            ProviderError::ContextLengthExceeded => ErrorClass::InvalidRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> OpenAiChatGptProvider {
        OpenAiChatGptProvider::new(
            OAuthCredential {
                provider: agent_auth::ProviderId::OpenAiChatGpt,
                access: agent_auth::Secret::new("AT"),
                refresh: agent_auth::Secret::new("RT"),
                expires_at: u64::MAX,
                account_id: Some("acct".into()),
            },
            DEFAULT_MAX_CONTEXT,
            None,
        )
    }

    #[test]
    fn capabilities_are_sse_stateless() {
        let p = provider();
        let c = p.capabilities();
        assert!(!c.server_side_state, "SSE stateless → mappe le canonique");
        assert!(c.tools && c.reasoning);
        assert_eq!(p.kind(), ProviderKind::OpenAiChatGpt);
    }

    #[test]
    fn classify_error_taxonomy() {
        let p = provider();
        let http = |s| ProviderError::Http {
            status: s,
            message: String::new(),
        };
        assert!(matches!(
            p.classify_error(&http(401)),
            ErrorClass::Auth(AuthError::Invalid)
        ));
        assert!(matches!(
            p.classify_error(&http(429)),
            ErrorClass::RateLimited
        ));
        assert!(matches!(
            p.classify_error(&http(529)),
            ErrorClass::Overloaded(529)
        ));
        assert!(matches!(
            p.classify_error(&http(503)),
            ErrorClass::Retryable
        ));
        assert!(matches!(
            p.classify_error(&http(400)),
            ErrorClass::InvalidRequest
        ));
        assert!(matches!(
            p.classify_error(&ProviderError::Transport("x".into())),
            ErrorClass::Retryable
        ));
    }
}
