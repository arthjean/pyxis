//! `agent-core` — le cœur headless de Pyxis : boucle d'agent en state machine,
//! types canoniques, budget de contexte, compaction. Émet UNIQUEMENT des
//! `AgentEvent` (jamais d'ANSI). Testable sans API/terminal/disque réels
//! (deps injectables). Invariants : ARCHITECTURE.md « Invariants à ne jamais
//! violer ».
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod budget;
pub mod clock;
pub mod compaction;
pub mod deps;
pub mod error;
pub mod event;
pub mod guardrail;
pub mod message;
pub mod provider;
pub mod session;
pub mod tools;
pub mod transition;

pub use agent::{AgentContext, HeadlessEnd, HeadlessResult, RunConfig, run_agent, run_headless};
pub use budget::ContextBudget;
pub use compaction::CompactKind;
pub use deps::Deps;
pub use error::{AgentError, ProviderFailure, ProviderFailureKind};
pub use event::AgentEvent;
pub use guardrail::{CostBudget, LoopDecision, LoopGuard, UsageBudget};
pub use message::{ContentBlock, Message, Role, ToolErrorKind};
pub use provider::{
    AuthError, CacheCapabilities, Capabilities, CapabilityLimits, ErrorClass, Provider,
    ProviderError, ProviderKind, ReasoningCapabilities, StopReason, StreamEvent, TokenUsage,
    ToolCallingCapabilities,
};
pub use session::{Session, SessionEntry, SessionError};
pub use tools::{ToolDispatch, ToolDispatchEvent, ToolEventSink, ToolInvocation, ToolOutcome};

#[cfg(test)]
mod loop_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use agent_tokenizer::HeuristicCounter;
    use futures_util::stream::BoxStream;
    use futures_util::{StreamExt, pin_mut};

    use crate::clock::Clock;
    use crate::compaction::CompactKind;
    use crate::message::{ContentBlock, Message};
    use crate::provider::{
        AuthError, CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider,
        ProviderError, ProviderKind, StopReason, StreamEvent, TokenUsage,
    };
    use crate::session::{Session, SessionError};
    use crate::tools::{ToolDispatch, ToolEventSink, ToolInvocation, ToolOutcome};
    use crate::{AgentContext, AgentEvent, Deps, RunConfig, run_agent, run_headless};

    // ───────── doubles de test (injectés via Deps) ─────────

    enum MockTurn {
        Stream(Vec<StreamEvent>),
        /// Erreur à l'OUVERTURE du stream.
        Err(ProviderError),
        /// Quelques events PUIS une erreur EN MILIEU de stream.
        StreamThenErr(Vec<StreamEvent>, ProviderError),
    }

    struct MockProvider {
        caps: Capabilities,
        turns: Mutex<VecDeque<MockTurn>>,
        summary: String,
        summary_usage: TokenUsage,
        summary_fails: bool,
        log: Arc<Mutex<Vec<&'static str>>>,
        refreshes: Arc<Mutex<u32>>,
        /// Capture les `messages` de chaque requête (US-028 : vérifier l'injection
        /// éphémère sans toucher au transcript persistant).
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        request_models: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAiChatGpt
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn max_context_for_model(&self, model: &str) -> u32 {
            if model == "small-context" {
                1000
            } else {
                self.caps.max_context
            }
        }
        async fn stream(
            &self,
            req: CanonicalRequest,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.log.lock().unwrap().push("stream");
            self.request_models.lock().unwrap().push(req.model.clone());
            self.requests.lock().unwrap().push(req.messages.clone());
            match self.turns.lock().unwrap().pop_front() {
                Some(MockTurn::Stream(evs)) => Ok(Box::pin(futures_util::stream::iter(
                    evs.into_iter().map(Ok),
                ))),
                Some(MockTurn::StreamThenErr(evs, err)) => {
                    let mut items: Vec<Result<StreamEvent, ProviderError>> =
                        evs.into_iter().map(Ok).collect();
                    items.push(Err(err));
                    Ok(Box::pin(futures_util::stream::iter(items)))
                }
                Some(MockTurn::Err(e)) => Err(e),
                None => Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                    StreamEvent::Done {
                        stop: StopReason::EndTurn,
                    },
                )]))),
            }
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
        ) -> Result<CanonicalResponse, ProviderError> {
            self.log.lock().unwrap().push("complete");
            if self.summary_fails {
                return Err(ProviderError::Transport("résumé KO".into()));
            }
            Ok(CanonicalResponse {
                content: vec![ContentBlock::Text {
                    text: self.summary.clone(),
                }],
                usage: self.summary_usage,
                stop: StopReason::EndTurn,
            })
        }
        fn classify_error(&self, err: &ProviderError) -> ErrorClass {
            match err {
                ProviderError::Http { status: 429, .. } => ErrorClass::RateLimited,
                ProviderError::Http { status: 529, .. } => ErrorClass::Overloaded(529),
                ProviderError::Http {
                    status: 401,
                    message,
                    ..
                } if message.contains("expired") => ErrorClass::Auth(AuthError::Expired),
                ProviderError::Http { status: 401, .. } => ErrorClass::Auth(AuthError::Invalid),
                ProviderError::Http { status: 400, .. } => ErrorClass::InvalidRequest,
                _ => ErrorClass::Retryable,
            }
        }
        async fn refresh_auth(&self) -> Result<(), ProviderError> {
            *self.refreshes.lock().unwrap() += 1;
            Ok(())
        }
    }

    struct InMemorySession {
        synced: Mutex<Vec<Message>>,
        cursor: Mutex<usize>,
        boundaries: Mutex<Vec<CompactKind>>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl Session for InMemorySession {
        async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
            self.log.lock().unwrap().push("sync");
            let mut cur = self.cursor.lock().unwrap();
            let start = (*cur).min(messages.len());
            let mut s = self.synced.lock().unwrap();
            for m in &messages[start..] {
                s.push(m.clone());
            }
            *cur = messages.len();
            Ok(())
        }
        async fn checkpoint(
            &self,
            kind: CompactKind,
            messages: &[Message],
        ) -> Result<(), SessionError> {
            self.boundaries.lock().unwrap().push(kind);
            // le transcript a été remplacé par le résumé : on resync.
            let mut s = self.synced.lock().unwrap();
            s.clear();
            s.extend_from_slice(messages);
            *self.cursor.lock().unwrap() = messages.len();
            Ok(())
        }
        async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
            let mut s = self.synced.lock().unwrap();
            for message in &mut *s {
                message
                    .content
                    .retain(|block| !matches!(block, ContentBlock::EncryptedReasoning { .. }));
            }
            Ok(())
        }
        async fn record_file_snapshot(
            &self,
            _snapshot: crate::session::FileSnapshot,
        ) -> Result<(), SessionError> {
            Ok(())
        }
    }

    struct NoopClock;
    #[async_trait::async_trait]
    impl Clock for NoopClock {
        fn now_ms(&self) -> u64 {
            0
        }
        async fn sleep(&self, _dur: std::time::Duration) {}
    }

    struct EchoTools;
    #[async_trait::async_trait]
    impl ToolDispatch for EchoTools {
        async fn dispatch(
            &self,
            calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            calls
                .into_iter()
                .map(|c| ToolOutcome {
                    id: c.id,
                    content: format!("echo:{}", c.input),
                    is_error: false,
                    untrusted: true,
                    error_kind: None,
                })
                .collect()
        }
    }

    struct MissingTools;
    #[async_trait::async_trait]
    impl ToolDispatch for MissingTools {
        async fn dispatch(
            &self,
            _calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            Vec::new()
        }
    }

    // ───────── harnais ─────────

    struct Harness {
        log: Arc<Mutex<Vec<&'static str>>>,
        refreshes: Arc<Mutex<u32>>,
        boundaries: Arc<InMemorySession>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        request_models: Arc<Mutex<Vec<String>>>,
        deps: Deps,
    }

    fn harness(turns: Vec<MockTurn>, summary_fails: bool, max_context: u32) -> Harness {
        harness_with_summary_usage(turns, summary_fails, max_context, TokenUsage::default())
    }

    fn harness_with_summary_usage(
        turns: Vec<MockTurn>,
        summary_fails: bool,
        max_context: u32,
        summary_usage: TokenUsage,
    ) -> Harness {
        let log = Arc::new(Mutex::new(Vec::new()));
        let refreshes = Arc::new(Mutex::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_models = Arc::new(Mutex::new(Vec::new()));
        let session = Arc::new(InMemorySession {
            synced: Mutex::new(Vec::new()),
            cursor: Mutex::new(0),
            boundaries: Mutex::new(Vec::new()),
            log: Arc::clone(&log),
        });
        let provider = Arc::new(MockProvider {
            caps: Capabilities {
                vision: false,
                tools: true,
                prompt_caching: false,
                reasoning: false,
                server_side_state: false,
                max_context,
                tool_calling: crate::provider::ToolCallingCapabilities {
                    parallel_tool_calls: true,
                    strict_json_schema: false,
                },
                ..Capabilities::default()
            },
            turns: Mutex::new(turns.into()),
            summary: "RÉSUMÉ".to_string(),
            summary_usage,
            summary_fails,
            log: Arc::clone(&log),
            refreshes: Arc::clone(&refreshes),
            requests: Arc::clone(&requests),
            request_models: Arc::clone(&request_models),
        });
        let deps = Deps {
            provider,
            session: Arc::clone(&session) as Arc<dyn Session>,
            tokenizer: Arc::new(HeuristicCounter),
            clock: Arc::new(NoopClock),
            tools: Arc::new(EchoTools),
        };
        Harness {
            log,
            refreshes,
            boundaries: session,
            requests,
            request_models,
            deps,
        }
    }

    async fn drive(ctx: AgentContext, deps: Deps) -> Vec<AgentEvent> {
        let stream = run_agent(ctx, deps);
        pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    fn tool_turn(id: &str) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::ToolCallStart {
                id: id.into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: id.into(),
                args_json: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ])
    }

    fn text_turn(t: &str) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::TextDelta { text: t.into() },
            StreamEvent::Done {
                stop: StopReason::EndTurn,
            },
        ])
    }

    fn has_compacted(events: &[AgentEvent], kind: CompactKind) -> bool {
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted(k) if *k == kind))
    }

    // ───────── tests ─────────

    // US-006 AC1/AC3 : conversation multi-tours headless, sans Ratatui.
    #[tokio::test]
    async fn multi_turn_headless_runs_without_tui() {
        let h = harness(vec![tool_turn("c1"), text_turn("fini")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("fais un ls"));
        let res = run_headless(ctx, h.deps).await;
        assert!(res.text.contains("fini"));
        assert!(matches!(res.ended, crate::HeadlessEnd::EndTurn));
    }

    // US-006 AC2 : le message est persisté (sync) AVANT le 1er appel API.
    #[tokio::test]
    async fn transcript_synced_before_stream() {
        let h = harness(vec![text_turn("ok")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("salut"));
        let _ = run_headless(ctx, h.deps).await;
        let log = h.log.lock().unwrap().clone();
        let sync_at = log.iter().position(|e| *e == "sync");
        let stream_at = log.iter().position(|e| *e == "stream");
        assert!(sync_at.is_some() && stream_at.is_some());
        assert!(sync_at < stream_at, "sync doit précéder stream: {log:?}");
    }

    // US-024 : le DERNIER message assistant est syncé AVANT EndTurn — sinon
    // `/resume` perd la dernière réponse. Le sync final est delta-only (idempotent) :
    // `synced.len() == 2` prouve l'absence de doublon du message user déjà syncé.
    #[tokio::test]
    async fn final_assistant_turn_synced_before_endturn() {
        let h = harness(vec![text_turn("réponse finale")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("question"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        let synced = h.boundaries.synced.lock().unwrap();
        assert_eq!(
            synced.len(),
            2,
            "user + assistant final, sans doublon: {synced:?}"
        );
        let last = synced.last().unwrap();
        assert_eq!(last.role, crate::message::Role::Assistant);
        assert!(
            last.text().contains("réponse finale"),
            "le dernier message persisté doit être la réponse finale: {synced:?}"
        );
    }

    // US-028 : les messages de contexte (AGENTS.md + env) sont préfixés à CHAQUE
    // requête mais JAMAIS persistés ni accumulés dans le transcript (rechargés).
    #[tokio::test]
    async fn context_messages_injected_per_request_never_persisted() {
        let h = harness(vec![tool_turn("c1"), text_turn("fini")], false, 100_000);
        let ctx = AgentContext::new("mock")
            .with_context_messages(vec![
                Message::user("# AGENTS.md instructions\nCTX_AGENTS"),
                Message::user("<environment>CTX_ENV</environment>"),
            ])
            .push(Message::user("fais X"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        // 1. Chaque requête envoyée au provider commence par les 2 messages de contexte.
        let reqs = h.requests.lock().unwrap();
        assert!(reqs.len() >= 2, "au moins 2 tours");
        for (i, msgs) in reqs.iter().enumerate() {
            assert!(
                msgs[0].text().contains("CTX_AGENTS") && msgs[1].text().contains("CTX_ENV"),
                "tour {i} : le contexte doit préfixer la requête"
            );
            assert!(
                msgs.iter()
                    .filter(|m| m.text().contains("CTX_AGENTS"))
                    .count()
                    == 1,
                "tour {i} : pas d'accumulation du contexte (une seule occurrence)"
            );
        }

        // 2. Le transcript persistant NE contient PAS les messages de contexte.
        let synced = h.boundaries.synced.lock().unwrap();
        assert!(
            !synced
                .iter()
                .any(|m| m.text().contains("CTX_AGENTS") || m.text().contains("CTX_ENV")),
            "le contexte éphémère ne doit jamais être persisté: {synced:?}"
        );
    }

    #[tokio::test]
    async fn ephemeral_messages_suffix_request_never_persisted() {
        let h = harness(vec![text_turn("fini")], false, 100_000);
        let ctx = AgentContext::new("mock")
            .with_context_messages(vec![Message::user("CTX")])
            .with_ephemeral_messages(vec![Message::user("CONTROL")])
            .push(Message::user("humain"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        let reqs = h.requests.lock().unwrap();
        let first = reqs.first().expect("requête provider");
        assert_eq!(first[0].text(), "CTX");
        assert_eq!(first[first.len() - 2].text(), "humain");
        assert_eq!(first[first.len() - 1].text(), "CONTROL");

        let synced = h.boundaries.synced.lock().unwrap();
        assert!(synced.iter().any(|m| m.text() == "humain"));
        assert!(!synced.iter().any(|m| m.text() == "CONTROL"));
    }

    // US-006 AC4 + US-008 AC4 : erreur de contexte → withholding → compaction
    // REACTIVE, pas de terminaison prématurée, la conversation continue.
    #[tokio::test]
    async fn context_error_triggers_withholding_and_reactive_compaction() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::ContextLengthExceeded),
                text_turn("repris après compaction"),
            ],
            false,
            100_000,
        );
        // historique réel (≥ 2 messages) → la compaction a de quoi résumer.
        let ctx = AgentContext::new("mock")
            .push(Message::user("contexte initial"))
            .push(Message::assistant_text("compris"))
            .push(Message::user("tâche longue"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "compaction réactive attendue: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("repris"))),
            "la conversation doit continuer après recovery"
        );
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        assert!(
            h.boundaries
                .boundaries
                .lock()
                .unwrap()
                .contains(&CompactKind::Reactive)
        );
    }

    // US-008 AC2 : seuil autocompact franchi → résumé proactif (Compacted::Auto).
    #[tokio::test]
    async fn autocompaction_triggers_on_budget_threshold() {
        // fenêtre 1000, réserve (max_output) 200 → auto à 640. Un gros user (~3000
        // octets ≈ 750 tokens heuristiques) dépasse le seuil dès l'estimation.
        let huge = "x".repeat(3000);
        let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 1000);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                max_output_tokens: 200,
                ..RunConfig::default()
            })
            .push(Message::user(huge));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Auto),
            "autocompaction attendue: {events:?}"
        );
    }

    // US-007 AC3 : provider SANS usage en stream → le fallback tokenizer alimente
    // le seuil, l'autocompaction se déclenche quand même.
    #[tokio::test]
    async fn fallback_tokenizer_feeds_threshold_without_usage() {
        let huge = "y".repeat(3000); // ~750 tokens, aucun Usage émis par le mock
        let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 1000);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                max_output_tokens: 200,
                ..RunConfig::default()
            })
            .push(Message::user(huge));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Auto),
            "le seuil doit être alimenté par l'estimation locale: {events:?}"
        );
    }

    // US-008 AC3 : échecs d'autocompact répétés → circuit breaker (pas de boucle).
    #[tokio::test]
    async fn circuit_breaker_stops_repeated_autocompact_failures() {
        let huge = "z".repeat(3000);
        let h = harness(
            vec![tool_turn("c1")], // un tour d'outil, puis on boucle sur l'autocompact
            true,                  // summary_fails → full_compact échoue toujours
            1000,
        );
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                max_output_tokens: 200,
                compaction_breaker_limit: 3,
                ..RunConfig::default()
            })
            .push(Message::user(huge));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(
                    crate::AgentError::CompactionCircuitBreaker(_)
                ))
            ),
            "circuit breaker attendu en fin: {events:?}"
        );
    }

    // US-006 AC4 (unhappy) : si la compaction réactive ÉCHOUE, l'erreur de
    // contexte est propagée (ContextUnrecoverable) — pas de fin prématurée avant
    // l'échec confirmé du recovery.
    #[tokio::test]
    async fn recovery_failure_propagates_context_unrecoverable() {
        let h = harness(
            vec![MockTurn::Err(ProviderError::ContextLengthExceeded)],
            true, // summary_fails → la compaction réactive échoue (provider.complete KO)
            100_000,
        );
        // historique ≥ 2 messages : provider.complete EST appelé (et échoue), ce
        // n'est pas le guard "rien à résumer" qui court-circuite.
        let ctx = AgentContext::new("mock")
            .push(Message::user("contexte"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("tâche"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(crate::AgentError::ContextUnrecoverable(
                    _
                )))
            ),
            "échec de recovery doit propager ContextUnrecoverable: {events:?}"
        );
    }

    // US-008 AC4 (distinct) : un 413 reçu EN MILIEU de stream déclenche la
    // compaction réactive (chemin distinct de l'échec à l'ouverture).
    #[tokio::test]
    async fn http_413_midstream_triggers_reactive_compaction() {
        let h = harness(
            vec![
                MockTurn::StreamThenErr(
                    vec![StreamEvent::TextDelta {
                        text: "partiel".into(),
                    }],
                    ProviderError::Http {
                        status: 413,
                        message: "too long".into(),
                        retry_after_ms: None,
                    },
                ),
                text_turn("repris après 413"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock")
            .push(Message::user("contexte"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("tâche"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "réactive attendue: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("repris")))
        );
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    }

    #[tokio::test]
    async fn stream_without_terminal_fails_closed() {
        let h = harness(
            vec![MockTurn::Stream(vec![StreamEvent::TextDelta {
                text: "partiel".into(),
            }])],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock").push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::StreamReset)),
            "les deltas visibles doivent être retirés: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(crate::AgentError::Provider(_)))
            ),
            "fin sans terminal doit fail-closed: {events:?}"
        );
    }

    #[tokio::test]
    async fn invalid_context_geometry_fails_before_provider_call() {
        let h = harness(vec![text_turn("jamais")], false, 100);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                max_output_tokens: 100,
                ..RunConfig::default()
            })
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.first(),
                Some(AgentEvent::Error(crate::AgentError::InvalidRequest(_)))
            ),
            "géométrie contexte invalide attendue: {events:?}"
        );
        assert!(
            !h.log.lock().unwrap().contains(&"stream"),
            "le provider ne doit pas être appelé"
        );
    }

    #[tokio::test]
    async fn auth_expired_refreshes_then_retries_opening_stream() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 401,
                    message: "access token expired".into(),
                    retry_after_ms: None,
                }),
                text_turn("ok"),
            ],
            false,
            100_000,
        );
        let refreshes = Arc::clone(&h.refreshes);
        let log = Arc::clone(&h.log);
        let ctx = AgentContext::new("mock").push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        assert_eq!(*refreshes.lock().unwrap(), 1);
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn overload_opening_stream_switches_to_configured_fallback_model() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 529,
                    message: "overloaded".into(),
                    retry_after_ms: None,
                }),
                text_turn("ok"),
            ],
            false,
            100_000,
        );
        let request_models = Arc::clone(&h.request_models);
        let ctx = AgentContext::new("primary")
            .with_config(RunConfig {
                overload_fallback_model: Some("fallback".into()),
                ..RunConfig::default()
            })
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        assert_eq!(
            *request_models.lock().unwrap(),
            vec!["primary".to_string(), "fallback".to_string()]
        );
    }

    #[tokio::test]
    async fn overload_fallback_rebuilds_context_budget_for_new_model() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 529,
                    message: "overloaded".into(),
                    retry_after_ms: None,
                }),
                text_turn("ok"),
            ],
            false,
            100_000,
        );
        let request_models = Arc::clone(&h.request_models);
        let ctx = AgentContext::new("primary")
            .with_config(RunConfig {
                max_output_tokens: 200,
                overload_fallback_model: Some("small-context".into()),
                ..RunConfig::default()
            })
            .push(Message::user("historique très long"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("x".repeat(3000)));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Auto),
            "la petite fenêtre fallback doit déclencher l'auto-compaction: {events:?}"
        );
        assert_eq!(
            *request_models.lock().unwrap(),
            vec!["primary".to_string(), "small-context".to_string()]
        );
    }

    #[tokio::test]
    async fn retry_after_visible_delta_resets_headless_output() {
        let h = harness(
            vec![
                MockTurn::StreamThenErr(
                    vec![StreamEvent::TextDelta {
                        text: "fantôme ".into(),
                    }],
                    ProviderError::Stream("reset".into()),
                ),
                text_turn("final"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock").push(Message::user("go"));
        let res = run_headless(ctx, h.deps).await;
        assert_eq!(res.text, "final");
        assert!(matches!(res.ended, crate::HeadlessEnd::EndTurn));
    }

    #[tokio::test]
    async fn maxtokens_plain_text_is_exhausted_not_success() {
        let h = harness(
            vec![MockTurn::Stream(vec![
                StreamEvent::TextDelta {
                    text: "tronqué".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::MaxTokens,
                },
            ])],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock").push(Message::user("go"));
        let res = run_headless(ctx, h.deps).await;
        assert_eq!(res.text, "");
        assert!(matches!(
            res.ended,
            crate::HeadlessEnd::Exhausted(ExhaustReason::MaxOutputTokens {
                visible_output: true
            })
        ));
    }

    #[tokio::test]
    async fn dispatcher_missing_outcome_is_contract_error() {
        let mut h = harness(vec![tool_turn("c1")], false, 100_000);
        h.deps.tools = Arc::new(MissingTools);
        let ctx = AgentContext::new("mock").push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(crate::AgentError::Provider(_)))
            ),
            "outcome manquant doit casser le contrat: {events:?}"
        );
    }

    // US-006/008 : MaxTokens en plein tool_call → withholding (Recover) → réactive,
    // l'intention d'outil n'est pas silencieusement jetée.
    #[tokio::test]
    async fn maxtokens_midtool_recovers_in_loop() {
        let h = harness(
            vec![
                MockTurn::Stream(vec![
                    StreamEvent::ToolCallStart {
                        id: "c1".into(),
                        name: "bash".into(),
                    },
                    StreamEvent::ToolCallDelta {
                        id: "c1".into(),
                        args_json: "{\"cm".into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::MaxTokens,
                    },
                ]),
                text_turn("régénéré"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock")
            .push(Message::user("contexte initial"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("fais X"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "réactive attendue: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("régénéré")))
        );
    }

    // US-008 AC1 : microcompaction déclenchée DANS la boucle (seuil micro 70 %,
    // sous l'auto 80 %) → Compacted(Micro), sans Auto.
    #[tokio::test]
    async fn microcompaction_triggers_in_loop_below_auto() {
        // fenêtre 1000, réserve 200 → micro 560, auto 640. usage=600 ∈ [560,640).
        let turn = MockTurn::Stream(vec![
            StreamEvent::Usage {
                usage: TokenUsage {
                    input: 600,
                    output: 5,
                },
            },
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{}".into(),
            },
            StreamEvent::ToolCallEnd { id: "c1".into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]);
        let h = harness(vec![turn], false, 1000);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                max_output_tokens: 200,
                ..RunConfig::default()
            })
            .push(Message::user("go"))
            .push(Message::tool_result("a", "r1", false))
            .push(Message::tool_result("b", "r2", false))
            .push(Message::tool_result("c", "r3", false))
            .push(Message::tool_result("d", "r4", false));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Micro),
            "micro attendue: {events:?}"
        );
        assert!(
            !has_compacted(&events, CompactKind::Auto),
            "pas d'auto sous le seuil: {events:?}"
        );
    }

    // ───────── US-014 : loop guardrails + budgets (kill-switch) ─────────

    use crate::transition::ExhaustReason;

    /// Tour d'outil émettant un `usage` explicite (pour piloter le budget).
    fn tool_turn_usage(id: &str, input: u32, output: u32) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::Usage {
                usage: TokenUsage { input, output },
            },
            StreamEvent::ToolCallStart {
                id: id.into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: id.into(),
                args_json: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ])
    }

    // US-014 AC1 : même outil + mêmes args répétés → signal explicite à l'agent
    // (batch non exécuté), puis arrêt déterministe si la boucle persiste.
    #[tokio::test]
    async fn loop_guardrail_signals_then_aborts() {
        // Le modèle redemande le même `bash {cmd:ls}` indéfiniment.
        let h = harness(
            vec![
                tool_turn("c1"),
                tool_turn("c1"),
                tool_turn("c1"),
                tool_turn("c1"),
                tool_turn("c1"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock").push(Message::user("boucle"));
        let events = drive(ctx, h.deps).await;

        // Signal explicite renvoyé à l'agent (edge case #2).
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult(v) if v.content.contains("Boucle détectée") && v.is_error
            )),
            "un signal de boucle explicite est attendu: {events:?}"
        );
        // Arrêt déterministe au-delà du signal.
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { .. }))
            ),
            "fin attendue en ToolLoop: {events:?}"
        );
    }

    // US-014 : un batch DIFFÉRENT à chaque tour ne déclenche pas le garde-fou.
    #[tokio::test]
    async fn loop_guardrail_does_not_false_positive_on_distinct_calls() {
        let distinct = |id: &str, cmd: &str| {
            MockTurn::Stream(vec![
                StreamEvent::ToolCallStart {
                    id: id.into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolCallDelta {
                    id: id.into(),
                    args_json: format!("{{\"cmd\":\"{cmd}\"}}"),
                },
                StreamEvent::ToolCallEnd { id: id.into() },
                StreamEvent::Done {
                    stop: StopReason::ToolUse,
                },
            ])
        };
        let h = harness(
            vec![
                distinct("a", "ls"),
                distinct("b", "pwd"),
                distinct("c", "whoami"),
                text_turn("fini"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock").push(Message::user("trois actions distinctes"));
        let events = drive(ctx, h.deps).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Exhausted(ExhaustReason::ToolLoop { .. }))),
            "aucune boucle ne doit être détectée: {events:?}"
        );
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    }

    // US-014 AC2 : budget de tokens cumulé atteint → kill-switch (edge case #3).
    #[tokio::test]
    async fn token_budget_kill_switch_stops_run() {
        // Tour 1 consomme 150 tokens (>120) ; le tour 2 ne doit jamais démarrer.
        let h = harness(
            vec![tool_turn_usage("c1", 100, 50), text_turn("jamais atteint")],
            false,
            1_000_000,
        );
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(120),
                max_output_tokens: 10, // petit → l'estimation pré-tour ne stoppe pas le tour 1
                ..RunConfig::default()
            })
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                    spent: 150,
                    limit: 120
                }))
            ),
            "kill-switch budget attendu: {events:?}"
        );
        // Le 1er tour d'outil a bien eu lieu avant le kill-switch.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult(_)))
        );
    }

    #[tokio::test]
    async fn failed_stream_usage_counts_before_retry() {
        let h = harness(
            vec![
                MockTurn::StreamThenErr(
                    vec![StreamEvent::Usage {
                        usage: TokenUsage {
                            input: 100,
                            output: 50,
                        },
                    }],
                    ProviderError::Stream("reset".into()),
                ),
                text_turn("jamais atteint"),
            ],
            false,
            1_000_000,
        );
        let log = Arc::clone(&h.log);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(120),
                max_output_tokens: 10,
                ..RunConfig::default()
            })
            .push(Message::user("contexte"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                    spent: 150,
                    limit: 120
                }))
            ),
            "usage du stream échoué doit compter: {events:?}"
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1,
            "le retry doit être bloqué par le budget avant de rouvrir un stream"
        );
    }

    #[tokio::test]
    async fn compaction_usage_counts_against_token_budget() {
        let h = harness_with_summary_usage(
            vec![
                MockTurn::Err(ProviderError::ContextLengthExceeded),
                text_turn("jamais atteint"),
            ],
            false,
            100_000,
            TokenUsage {
                input: 100,
                output: 50,
            },
        );
        let log = Arc::clone(&h.log);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(120),
                max_output_tokens: 10,
                ..RunConfig::default()
            })
            .push(Message::user("contexte"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "compaction réactive attendue: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                    spent: 150,
                    limit: 120
                }))
            ),
            "usage de compaction doit compter: {events:?}"
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1,
            "aucun stream post-compaction ne doit démarrer après budget atteint"
        );
    }

    // US-014 AC3 : estimation PRÉ-tour → on stoppe AVANT un tour trop coûteux
    // (aucun appel provider émis).
    #[tokio::test]
    async fn pre_turn_estimate_stops_before_expensive_turn() {
        let h = harness(vec![text_turn("jamais")], false, 1_000_000);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(5), // < max_output → la projection dépasse d'emblée
                max_output_tokens: 100,
                ..RunConfig::default()
            })
            .push(Message::user("tâche"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.first(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget { .. }))
            ),
            "stop pré-tour attendu: {events:?}"
        );
        // Aucun stream provider ne doit avoir été ouvert.
        assert!(
            !h.log.lock().unwrap().contains(&"stream"),
            "le provider ne doit PAS être appelé: {:?}",
            h.log.lock().unwrap()
        );
    }
}
