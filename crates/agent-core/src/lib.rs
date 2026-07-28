//! `agent-core`: the headless core of Pyxis. State-machine agent loop,
//! canonical types, context budget, compaction. Emits ONLY
//! `AgentEvent` (never ANSI). Testable without a real API/terminal/disk
//! (injectable deps). Invariants: see ARCHITECTURE.md "Invariants never to
//! violate".
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod budget;
pub mod cancel;
pub mod clock;
pub mod compaction;
pub mod deps;
pub mod error;
pub mod event;
pub mod guardrail;
pub mod input;
pub mod message;
pub mod model;
pub mod prompt;
pub mod provider;
pub mod quota;
pub mod sandbox;
pub mod session;
pub mod step;
pub mod tools;
pub mod transition;

pub use agent::{
    AgentContext, HeadlessEnd, HeadlessResult, RunConfig, run_agent, run_headless,
    run_headless_observed,
};
pub use budget::ContextBudget;
pub use cancel::{Cancellable, CancellationToken};
pub use compaction::CompactKind;
pub use deps::Deps;
pub use error::{AgentError, ProviderFailure, ProviderFailureKind};
pub use event::{
    AgentEvent, CredentialRefreshOutcome, CredentialRefreshView, FileChange, FileDiffView,
    ModelTurnView, PlanStatus, PlanStep, PlanView, RetryScheduledView, TurnDiffView,
};
pub use guardrail::{CostBudget, LoopDecision, LoopGuard, UsageBudget};
pub use input::InputQueue;
pub use message::{
    ContentBlock, INTERRUPTED_TOOL_RESULT, Message, Role, ToolErrorKind, unanswered_tool_calls,
};
pub use prompt::{
    ContextBaseline, ContextTransition, ContextTransitionCause, PromptSnapshot, PromptSnapshotError,
};
pub use provider::{
    AuthError, CacheCapabilities, Capabilities, CapabilityLimits, ErrorClass, Provider,
    ProviderError, ProviderKind, ReasoningCapabilities, StopReason, StreamEvent, TokenUsage,
    ToolCallingCapabilities,
};
pub use quota::{QuotaSnapshot, QuotaWindow, format_unix_utc};
pub use sandbox::{SandboxPolicy, WritableRoot, WriteRefusal};
pub use session::{Session, SessionEntry, SessionError};
pub use step::{ContextFragmentKind, StepContextSource, StepFrame};
pub use tools::{
    MAX_MODEL_TOOL_RESULT_BYTES, ModelToolResult, StepToolPlan, ToolDispatch, ToolDispatchEvent,
    ToolDispatchSnapshot, ToolEventSink, ToolExecution, ToolImage, ToolInvocation, ToolOutcome,
    ToolResultStatus, ToolResultTruncation, TruncationStrategy,
};

#[cfg(test)]
mod loop_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use agent_tokenizer::HeuristicCounter;
    use futures_util::stream::BoxStream;
    use futures_util::{StreamExt, pin_mut};

    use crate::clock::Clock;
    use crate::compaction::CompactKind;
    use crate::message::{ContentBlock, INTERRUPTED_TOOL_RESULT, Message, unanswered_tool_calls};
    use crate::provider::{
        AuthError, CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider,
        ProviderError, ProviderKind, StopReason, StreamEvent, TokenUsage, ToolSpec,
    };
    use crate::session::{Session, SessionError};
    use crate::tools::{ToolDispatch, ToolEventSink, ToolInvocation, ToolOutcome};
    use crate::{
        AgentContext, AgentEvent, CancellationToken, Deps, RunConfig, run_agent, run_headless,
    };

    // ───────── test doubles (injected through Deps) ─────────

    enum MockTurn {
        Stream(Vec<StreamEvent>),
        /// Error when OPENING the stream.
        Err(ProviderError),
        /// A few events THEN an error MID-stream.
        StreamThenErr(Vec<StreamEvent>, ProviderError),
        /// US-001: the cancel signal fires while the stream is being consumed,
        /// exactly after the first delivered event.
        StreamCancelling(Vec<StreamEvent>, crate::CancellationToken),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RefreshBehavior {
        Succeed,
        Reject,
        Block,
    }

    struct MockProvider {
        caps: Capabilities,
        turns: Mutex<VecDeque<MockTurn>>,
        summary: String,
        summary_usage: TokenUsage,
        summary_fails: bool,
        log: Arc<Mutex<Vec<&'static str>>>,
        refreshes: Arc<Mutex<u32>>,
        refresh_behavior: Arc<Mutex<RefreshBehavior>>,
        refresh_started: Arc<tokio::sync::Semaphore>,
        /// Captures the `messages` of every request (US-028: check the ephemeral
        /// injection without touching the persisted transcript).
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        request_models: Arc<Mutex<Vec<String>>>,
        request_replays: Arc<Mutex<Vec<bool>>>,
        complete_models: Arc<Mutex<Vec<String>>>,
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
        /// US-001: only `windowed` has a window declared by the backend; the
        /// other slugs stay unknown, which is the nominal case as long as the
        /// catalog has not answered.
        fn context_window_for_model(&self, model: &str) -> Option<u32> {
            (model == "windowed").then_some(2_000)
        }
        async fn stream(
            &self,
            req: CanonicalRequest,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.log.lock().unwrap().push("stream");
            self.request_models.lock().unwrap().push(req.model.clone());
            self.request_replays
                .lock()
                .unwrap()
                .push(req.reasoning_replay);
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
                Some(MockTurn::StreamCancelling(evs, cancel)) => {
                    let mut delivered = 0usize;
                    Ok(Box::pin(
                        futures_util::stream::iter(evs.into_iter().map(Ok)).map(move |item| {
                            delivered += 1;
                            if delivered == 1 {
                                cancel.cancel();
                            }
                            item
                        }),
                    ))
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
            req: CanonicalRequest,
        ) -> Result<CanonicalResponse, ProviderError> {
            self.log.lock().unwrap().push("complete");
            self.complete_models.lock().unwrap().push(req.model);
            if self.summary_fails {
                return Err(ProviderError::Transport("summary failed".into()));
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
                ProviderError::Http { status: 401, .. } => ErrorClass::Auth(AuthError::Expired),
                ProviderError::Http {
                    status: 400,
                    message,
                    ..
                } if message.contains("encrypted_reasoning") => ErrorClass::ReasoningReplayRejected,
                ProviderError::Http { status: 400, .. } => ErrorClass::InvalidRequest,
                _ => ErrorClass::Retryable,
            }
        }
        async fn refresh_auth(&self) -> Result<(), ProviderError> {
            *self.refreshes.lock().unwrap() += 1;
            let behavior = *self.refresh_behavior.lock().unwrap();
            match behavior {
                RefreshBehavior::Succeed => Ok(()),
                RefreshBehavior::Reject => Err(ProviderError::Http {
                    status: 401,
                    message: "refresh rejected".into(),
                    retry_after_ms: None,
                }),
                RefreshBehavior::Block => {
                    self.refresh_started.add_permits(1);
                    std::future::pending().await
                }
            }
        }
    }

    struct InMemorySession {
        synced: Mutex<Vec<Message>>,
        cursor: Mutex<usize>,
        boundaries: Mutex<Vec<CompactKind>>,
        log: Arc<Mutex<Vec<&'static str>>>,
        fail_context_transition: Mutex<bool>,
        baseline: Mutex<Option<crate::ContextBaseline>>,
        context_transitions: Mutex<Vec<crate::ContextTransition>>,
    }

    #[async_trait::async_trait]
    impl Session for InMemorySession {
        fn context_baseline(&self) -> Option<crate::ContextBaseline> {
            self.baseline.lock().unwrap().clone()
        }

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
            // the transcript was replaced by the summary: resync.
            let mut s = self.synced.lock().unwrap();
            s.clear();
            s.extend_from_slice(messages);
            *self.cursor.lock().unwrap() = messages.len();
            *self.baseline.lock().unwrap() = None;
            Ok(())
        }
        async fn record_context_transition(
            &self,
            transition: crate::ContextTransition,
        ) -> Result<(), SessionError> {
            if *self.fail_context_transition.lock().unwrap() {
                return Err(SessionError::Io(
                    "injected context transition failure".into(),
                ));
            }
            *self.baseline.lock().unwrap() = Some(transition.to.clone());
            self.context_transitions.lock().unwrap().push(transition);
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

    struct BlockingClock {
        started: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl Clock for BlockingClock {
        fn now_ms(&self) -> u64 {
            0
        }

        async fn sleep(&self, _dur: std::time::Duration) {
            self.started.add_permits(1);
            std::future::pending().await
        }
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
                    images: Vec::new(),
                    ..ToolOutcome::new(c.id, format!("echo:{}", c.input), false, true, None)
                })
                .collect()
        }
    }

    /// US-015: tools that publish output fragments before their result.
    struct StreamingTools;
    #[async_trait::async_trait]
    impl ToolDispatch for StreamingTools {
        async fn dispatch(
            &self,
            calls: Vec<ToolInvocation>,
            events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            calls
                .into_iter()
                .map(|c| {
                    events.emit(crate::tools::ToolDispatchEvent::OutputDelta {
                        id: c.id.clone(),
                        chunk: "progression...\n".into(),
                    });
                    ToolOutcome {
                        images: Vec::new(),
                        ..ToolOutcome::new(c.id, format!("echo:{}", c.input), false, true, None)
                    }
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

    /// US-001: tools that signal cancellation then NEVER hand back control:
    /// the loop must take control back without waiting for the dispatch.
    struct HangingTools(crate::CancellationToken);
    #[async_trait::async_trait]
    impl ToolDispatch for HangingTools {
        async fn dispatch(
            &self,
            _calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            self.0.cancel();
            std::future::pending().await
        }
    }

    /// Edge case #2: cancellation lands in the window between the end of the
    /// dispatch and persistence: the REAL results must survive.
    struct RacingTools(crate::CancellationToken);
    #[async_trait::async_trait]
    impl ToolDispatch for RacingTools {
        async fn dispatch(
            &self,
            calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            self.0.cancel();
            calls
                .into_iter()
                .map(|c| ToolOutcome {
                    images: Vec::new(),
                    ..ToolOutcome::new(c.id, "real output".into(), false, true, None)
                })
                .collect()
        }
    }

    /// Sweeps the cancellation window around a dispatch: `yields` yield points
    /// before the signal, then termination (real results) or blocking
    /// (abandoned calls).
    struct DelayedCancelTools {
        cancel: crate::CancellationToken,
        yields: usize,
        finish: bool,
    }
    #[async_trait::async_trait]
    impl ToolDispatch for DelayedCancelTools {
        async fn dispatch(
            &self,
            calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            for _ in 0..self.yields {
                tokio::task::yield_now().await;
            }
            self.cancel.cancel();
            if !self.finish {
                std::future::pending::<()>().await;
            }
            calls
                .into_iter()
                .map(|c| ToolOutcome {
                    images: Vec::new(),
                    ..ToolOutcome::new(c.id, "real output".into(), false, true, None)
                })
                .collect()
        }
    }

    // ───────── harness ─────────

    struct Harness {
        log: Arc<Mutex<Vec<&'static str>>>,
        refreshes: Arc<Mutex<u32>>,
        refresh_behavior: Arc<Mutex<RefreshBehavior>>,
        refresh_started: Arc<tokio::sync::Semaphore>,
        boundaries: Arc<InMemorySession>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        request_models: Arc<Mutex<Vec<String>>>,
        request_replays: Arc<Mutex<Vec<bool>>>,
        complete_models: Arc<Mutex<Vec<String>>>,
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
        let refresh_behavior = Arc::new(Mutex::new(RefreshBehavior::Succeed));
        let refresh_started = Arc::new(tokio::sync::Semaphore::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_models = Arc::new(Mutex::new(Vec::new()));
        let request_replays = Arc::new(Mutex::new(Vec::new()));
        let complete_models = Arc::new(Mutex::new(Vec::new()));
        let session = Arc::new(InMemorySession {
            synced: Mutex::new(Vec::new()),
            cursor: Mutex::new(0),
            boundaries: Mutex::new(Vec::new()),
            log: Arc::clone(&log),
            fail_context_transition: Mutex::new(false),
            baseline: Mutex::new(None),
            context_transitions: Mutex::new(Vec::new()),
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
                    freeform_tools: false,
                },
                ..Capabilities::default()
            },
            turns: Mutex::new(turns.into()),
            summary: "SUMMARY".to_string(),
            summary_usage,
            summary_fails,
            log: Arc::clone(&log),
            refreshes: Arc::clone(&refreshes),
            refresh_behavior: Arc::clone(&refresh_behavior),
            refresh_started: Arc::clone(&refresh_started),
            requests: Arc::clone(&requests),
            request_models: Arc::clone(&request_models),
            request_replays: Arc::clone(&request_replays),
            complete_models: Arc::clone(&complete_models),
        });
        let deps = Deps {
            provider,
            session: Arc::clone(&session) as Arc<dyn Session>,
            tokenizer: Arc::new(HeuristicCounter),
            clock: Arc::new(NoopClock),
            tools: Arc::new(EchoTools),
            cancel: crate::CancellationToken::new(),
        };
        Harness {
            log,
            refreshes,
            refresh_behavior,
            refresh_started,
            boundaries: session,
            requests,
            request_models,
            request_replays,
            complete_models,
            deps,
        }
    }

    fn with_mock_tool(mut ctx: AgentContext) -> AgentContext {
        if ctx.tools.is_empty() {
            ctx.tools.push(ToolSpec::function(
                "bash",
                "mock shell",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" }
                    },
                    "required": ["cmd"],
                    "additionalProperties": false
                }),
            ));
        }
        ctx
    }

    async fn drive(ctx: AgentContext, deps: Deps) -> Vec<AgentEvent> {
        let stream = run_agent(with_mock_tool(ctx), deps);
        pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    fn tool_turn(id: &str) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::tool_call_start(id, "bash"),
            StreamEvent::ToolCallDelta {
                id: id.into(),
                input_delta: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ])
    }

    fn tool_turn_n(ids: &[&str]) -> MockTurn {
        let mut events = Vec::new();
        for id in ids {
            events.push(StreamEvent::tool_call_start(*id, "bash"));
            events.push(StreamEvent::ToolCallEnd { id: (*id).into() });
        }
        events.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
        });
        MockTurn::Stream(events)
    }

    fn text_turn(t: &str) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::TextDelta { text: t.into() },
            StreamEvent::Done {
                stop: StopReason::EndTurn,
            },
        ])
    }

    fn resolved_runtime(
        slug: &str,
        fingerprint: char,
        comp_hash: &str,
        replay: crate::model::ReasoningReplaySupport,
    ) -> crate::model::ResolvedModelRuntime {
        crate::model::ResolvedModelRuntime {
            slug: slug.into(),
            source: crate::model::ModelRuntimeSource::Embedded {
                version: "test".into(),
            },
            instructions: format!("{slug} instructions"),
            fingerprint: fingerprint.to_string().repeat(64),
            context_window: 100_000,
            auto_compact_token_limit: 90_000,
            input_modalities: vec![crate::model::InputModality::Text],
            reasoning_effort: Some("medium".into()),
            supports_verbosity: true,
            verbosity: Some("low".into()),
            supports_parallel_tool_calls: false,
            reasoning_replay: replay,
            responses_dialect: crate::model::ResponsesDialect::Standard,
            tool_mode: crate::model::ModelToolMode::Direct,
            truncation: crate::model::TruncationPolicy {
                mode: crate::model::TruncationMode::Tokens,
                limit: 1_000,
            },
            retry: crate::model::ModelRetryPolicy {
                max_attempts: 2,
                backoff_base_ms: 1,
            },
            max_output_tokens: 1_024,
            comp_hash: Some(comp_hash.into()),
        }
    }

    fn baseline(runtime: &crate::model::ResolvedModelRuntime) -> crate::ContextBaseline {
        crate::ContextBaseline {
            profile_fingerprint: runtime.fingerprint.clone(),
            model_slug: runtime.slug.clone(),
            comp_hash: runtime.comp_hash.clone(),
            instructions_fingerprint: "1".repeat(64),
            project_context_fingerprint: "2".repeat(64),
            skills_fingerprint: "3".repeat(64),
            tool_plan_fingerprint: "4".repeat(64),
        }
    }

    fn has_compacted(events: &[AgentEvent], kind: CompactKind) -> bool {
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted(k) if *k == kind))
    }

    // ───────── tests ─────────

    // US-006 AC1/AC3: headless multi-turn conversation, without Ratatui.
    #[tokio::test]
    async fn multi_turn_headless_runs_without_tui() {
        let h = harness(vec![tool_turn("c1"), text_turn("fini")], false, 100_000);
        let ctx = with_mock_tool(AgentContext::new("mock").push(Message::user("fais un ls")));
        let res = run_headless(ctx, h.deps).await;
        assert!(res.text.contains("fini"));
        assert!(matches!(res.ended, crate::HeadlessEnd::EndTurn));
    }

    // US-006 AC2: the message is persisted (sync) BEFORE the 1st API call.
    #[tokio::test]
    async fn transcript_synced_before_stream() {
        let h = harness(vec![text_turn("ok")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("hello"));
        let _ = run_headless(ctx, h.deps).await;
        let log = h.log.lock().unwrap().clone();
        let sync_at = log.iter().position(|e| *e == "sync");
        let stream_at = log.iter().position(|e| *e == "stream");
        assert!(sync_at.is_some() && stream_at.is_some());
        assert!(sync_at < stream_at, "sync should precede stream: {log:?}");
    }

    #[tokio::test]
    async fn failed_context_transition_prevents_provider_open() {
        let h = harness(vec![text_turn("must not run")], false, 100_000);
        *h.boundaries.fail_context_transition.lock().unwrap() = true;
        let events = drive(
            AgentContext::new("mock").push(Message::user("question")),
            h.deps,
        )
        .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Session(detail)))
                if detail.contains("context transition")
        ));
        assert!(h.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_descriptor_gates_reasoning_replay_for_the_turn() {
        let enabled = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 400,
                    message: "encrypted_reasoning is not supported".into(),
                    retry_after_ms: None,
                }),
                text_turn("done"),
            ],
            false,
            100_000,
        );
        let mut enabled_ctx = AgentContext::new("ignored").push(Message::user("task"));
        enabled_ctx.model_runtime = Some(resolved_runtime(
            "enabled",
            'a',
            "same",
            crate::model::ReasoningReplaySupport::Enabled,
        ));
        let events = drive(enabled_ctx, enabled.deps).await;
        assert_eq!(*enabled.request_replays.lock().unwrap(), vec![true, false]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ReasoningReplayDisabled { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RetryScheduled(view)
                if view.ordinal == 2
                    && view.cause == ErrorClass::ReasoningReplayRejected
                    && view.delay_ms == 0
        )));

        let disabled = harness(vec![text_turn("done")], false, 100_000);
        let mut disabled_ctx = AgentContext::new("ignored").push(Message::user("task"));
        disabled_ctx.model_runtime = Some(resolved_runtime(
            "disabled",
            'b',
            "same",
            crate::model::ReasoningReplaySupport::Disabled,
        ));
        let _ = drive(disabled_ctx, disabled.deps).await;
        assert_eq!(*disabled.request_replays.lock().unwrap(), vec![false]);
    }

    #[tokio::test]
    async fn comp_hash_change_compacts_with_old_model_before_new_sampling() {
        let old = resolved_runtime(
            "old-model",
            'a',
            "old-hash",
            crate::model::ReasoningReplaySupport::Enabled,
        );
        let new = resolved_runtime(
            "new-model",
            'b',
            "new-hash",
            crate::model::ReasoningReplaySupport::Enabled,
        );
        let h = harness(vec![text_turn("done")], false, 100_000);
        *h.boundaries.baseline.lock().unwrap() = Some(baseline(&old));
        let mut ctx = AgentContext::new("ignored")
            .push(Message::user("first"))
            .push(Message::assistant_text("answer"))
            .push(Message::user("next"));
        ctx.model_runtime = Some(new.clone());

        let events = drive(ctx, h.deps).await;
        assert!(has_compacted(&events, CompactKind::Auto));
        assert_eq!(*h.complete_models.lock().unwrap(), vec!["old-model"]);
        assert_eq!(*h.request_models.lock().unwrap(), vec!["new-model"]);
        let transitions = h.boundaries.context_transitions.lock().unwrap();
        let switch = transitions.last().expect("new baseline persisted");
        assert_eq!(
            switch
                .from
                .as_ref()
                .map(|from| from.profile_fingerprint.as_str()),
            Some(old.fingerprint.as_str())
        );
        assert_eq!(switch.to.profile_fingerprint, new.fingerprint);
        assert!(
            switch
                .causes
                .contains(&crate::ContextTransitionCause::CompHashChanged)
        );
    }

    #[tokio::test]
    async fn overload_fallback_installs_the_resolved_contract() {
        let primary = resolved_runtime(
            "primary",
            'a',
            "compatible",
            crate::model::ReasoningReplaySupport::Enabled,
        );
        let mut fallback = resolved_runtime(
            "fallback",
            'b',
            "compatible",
            crate::model::ReasoningReplaySupport::Enabled,
        );
        fallback.supports_parallel_tool_calls = true;
        fallback.context_window = 50_000;
        fallback.auto_compact_token_limit = 40_000;
        fallback.fingerprint = "c".repeat(64);
        let h = harness(
            vec![
                MockTurn::Stream(vec![
                    StreamEvent::ReasoningReplayDisabled {
                        reason: "rejected once".into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::Continue,
                    },
                ]),
                MockTurn::Err(ProviderError::Http {
                    status: 529,
                    message: "overloaded".into(),
                    retry_after_ms: None,
                }),
                text_turn("fallback answer"),
            ],
            false,
            100_000,
        );
        let mut ctx = AgentContext::new("ignored")
            .push(Message::user("task"))
            .with_config(RunConfig {
                overload_fallback_model: Some("fallback".into()),
                overload_fallback_runtime: Some(fallback.clone()),
                ..RunConfig::default()
            });
        ctx.model_runtime = Some(primary);

        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        assert_eq!(
            *h.request_models.lock().unwrap(),
            vec!["primary", "primary", "fallback"]
        );
        assert_eq!(
            *h.request_replays.lock().unwrap(),
            vec![true, false, false],
            "a replay downgrade is latched for the whole turn, including fallback"
        );
        assert!(
            h.requests.lock().unwrap()[2]
                .iter()
                .any(|message| message.text().contains("<model_switch"))
        );
        assert_eq!(
            h.boundaries
                .context_transitions
                .lock()
                .unwrap()
                .last()
                .map(|transition| transition.to.profile_fingerprint.clone()),
            Some(fallback.fingerprint)
        );
    }

    // US-024: the LAST assistant message is synced BEFORE EndTurn, otherwise
    // `/resume` loses the last reply. The final sync is delta-only (idempotent):
    // `synced.len() == 2` proves the already-synced user message is not duplicated.
    #[tokio::test]
    async fn final_assistant_turn_synced_before_endturn() {
        let h = harness(vec![text_turn("final answer")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("question"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        let synced = h.boundaries.synced.lock().unwrap();
        assert_eq!(
            synced.len(),
            2,
            "user plus final assistant, without duplicate: {synced:?}"
        );
        let last = synced.last().unwrap();
        assert_eq!(last.role, crate::message::Role::Assistant);
        assert!(
            last.text().contains("final answer"),
            "the last persisted message should be the final answer: {synced:?}"
        );
    }

    #[tokio::test]
    async fn continuation_commits_assistant_and_resamples_without_user_input() {
        let first = MockTurn::Stream(vec![
            StreamEvent::TextDelta {
                text: "working".into(),
            },
            StreamEvent::Done {
                stop: StopReason::Continue,
            },
        ]);
        let h = harness(vec![first, text_turn("finished")], false, 100_000);
        let ctx = AgentContext::new("mock").push(Message::user("task"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ModelTurn(_)))
                .count(),
            2
        );
        let requests = h.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]
                .iter()
                .filter(|message| message.role == crate::message::Role::User)
                .count(),
            1,
            "continuation must not fabricate a user input"
        );
        assert!(
            requests[1]
                .iter()
                .any(|message| message.role == crate::message::Role::Assistant
                    && message.text().contains("working"))
        );
    }

    #[tokio::test]
    async fn repeated_continuations_exhaust_the_model_turn_budget() {
        let continuation = || {
            MockTurn::Stream(vec![
                StreamEvent::TextDelta {
                    text: "still working".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::Continue,
                },
            ])
        };
        let h = harness(vec![continuation(), continuation()], false, 100_000);
        let ctx = AgentContext::new("mock")
            .push(Message::user("task"))
            .with_config(RunConfig {
                max_turns: 2,
                ..RunConfig::default()
            });
        let events = drive(ctx, h.deps).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Exhausted(ExhaustReason::MaxTurns(2)))
        ));
        assert_eq!(h.requests.lock().unwrap().len(), 2);
    }

    // US-028: context messages (AGENTS.md + env) are prefixed to EVERY
    // request but NEVER persisted nor accumulated in the transcript (reloaded).
    #[tokio::test]
    async fn context_messages_injected_per_request_never_persisted() {
        let h = harness(vec![tool_turn("c1"), text_turn("done")], false, 100_000);
        let ctx = AgentContext::new("mock")
            .with_context_messages(vec![
                Message::user("# AGENTS.md instructions\nCTX_AGENTS"),
                Message::user("<environment>CTX_ENV</environment>"),
            ])
            .push(Message::user("do X"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        // 1. Every request sent to the provider starts with the 2 context messages.
        let reqs = h.requests.lock().unwrap();
        assert!(reqs.len() >= 2, "at least 2 turns");
        for (i, msgs) in reqs.iter().enumerate() {
            assert!(
                msgs[0].text().contains("CTX_AGENTS") && msgs[1].text().contains("CTX_ENV"),
                "turn {i}: context should prefix the request"
            );
            assert!(
                msgs.iter()
                    .filter(|m| m.text().contains("CTX_AGENTS"))
                    .count()
                    == 1,
                "turn {i}: no context accumulation, one occurrence only"
            );
        }

        // 2. The persisted transcript does NOT contain the context messages.
        let synced = h.boundaries.synced.lock().unwrap();
        assert!(
            !synced
                .iter()
                .any(|m| m.text().contains("CTX_AGENTS") || m.text().contains("CTX_ENV")),
            "ephemeral context should never be persisted: {synced:?}"
        );
    }

    #[tokio::test]
    async fn ephemeral_messages_suffix_request_never_persisted() {
        let h = harness(vec![text_turn("done")], false, 100_000);
        let ctx = AgentContext::new("mock")
            .with_context_messages(vec![Message::user("CTX")])
            .with_ephemeral_messages(vec![Message::user("CONTROL")])
            .push(Message::user("human"));
        let events = drive(ctx, h.deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));

        let reqs = h.requests.lock().unwrap();
        let first = reqs.first().expect("provider request");
        assert_eq!(first[0].text(), "CTX");
        assert_eq!(first[first.len() - 2].text(), "human");
        assert_eq!(first[first.len() - 1].text(), "CONTROL");

        let synced = h.boundaries.synced.lock().unwrap();
        assert!(synced.iter().any(|m| m.text() == "human"));
        assert!(!synced.iter().any(|m| m.text() == "CONTROL"));
    }

    // US-006 AC4 + US-008 AC4: context error -> withholding -> REACTIVE
    // compaction, no premature termination, the conversation goes on.
    #[tokio::test]
    async fn context_error_triggers_withholding_and_reactive_compaction() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::ContextLengthExceeded),
                text_turn("resumed after compaction"),
            ],
            false,
            100_000,
        );
        // real history (>= 2 messages) -> compaction has something to summarize.
        let ctx = AgentContext::new("mock")
            .push(Message::user("initial context"))
            .push(Message::assistant_text("compris"))
            .push(Message::user("long task"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "reactive compaction expected: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("resumed"))),
            "conversation should continue after recovery"
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

    // US-008 AC2: autocompact threshold crossed -> proactive summary (Compacted::Auto).
    #[tokio::test]
    async fn autocompaction_triggers_on_budget_threshold() {
        // window 1000, reserve (max_output) 200 -> auto at 640. A large user message
        // (~3000 bytes, roughly 750 heuristic tokens) crosses it at estimation time.
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
            "autocompaction expected: {events:?}"
        );
    }

    // US-007 AC3: provider WITHOUT usage in the stream -> the tokenizer fallback
    // feeds the threshold, autocompaction still triggers.
    #[tokio::test]
    async fn fallback_tokenizer_feeds_threshold_without_usage() {
        let huge = "y".repeat(3000); // ~750 tokens, no Usage emitted by the mock
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
            "threshold should be fed by the local estimate: {events:?}"
        );
    }

    // US-008 AC3: repeated autocompact failures -> circuit breaker (no loop).
    #[tokio::test]
    async fn circuit_breaker_stops_repeated_autocompact_failures() {
        let huge = "z".repeat(3000);
        let h = harness(
            vec![tool_turn("c1")], // one tool turn, then we loop on autocompact
            true,                  // summary_fails -> full_compact always fails
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
            "circuit breaker expected at the end: {events:?}"
        );
    }

    // US-006 AC4 (unhappy): if reactive compaction FAILS, the context error is
    // propagated (ContextUnrecoverable), no premature end before the recovery
    // failure is confirmed.
    #[tokio::test]
    async fn recovery_failure_propagates_context_unrecoverable() {
        let h = harness(
            vec![MockTurn::Err(ProviderError::ContextLengthExceeded)],
            true, // summary_fails -> reactive compaction fails (provider.complete KO)
            100_000,
        );
        // history >= 2 messages: provider.complete IS called (and fails), this
        // is not the "nothing to summarize" guard short-circuiting.
        let ctx = AgentContext::new("mock")
            .push(Message::user("context"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("task"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(crate::AgentError::ContextUnrecoverable(
                    _
                )))
            ),
            "recovery failure should propagate ContextUnrecoverable: {events:?}"
        );
    }

    // US-008 AC4 (distinct): a 413 received MID-stream triggers reactive
    // compaction (path distinct from the failure at open time).
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
                text_turn("resumed after 413"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock")
            .push(Message::user("context"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("task"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "reactive expected: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("resumed")))
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
            "visible deltas should be removed: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error(crate::AgentError::Provider(_)))
            ),
            "missing terminal event should fail closed: {events:?}"
        );
    }

    #[tokio::test]
    async fn invalid_context_geometry_fails_before_provider_call() {
        let h = harness(vec![text_turn("never")], false, 100);
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
            "invalid context geometry expected: {events:?}"
        );
        assert!(
            !h.log.lock().unwrap().contains(&"stream"),
            "provider should not be called"
        );
    }

    #[tokio::test]
    async fn auth_expired_refreshes_then_retries_opening_stream() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 401,
                    message: "backend wording without an expiry marker".into(),
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
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CredentialRefresh(view)
                if view.outcome == crate::CredentialRefreshOutcome::Started
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CredentialRefresh(view)
                if view.outcome == crate::CredentialRefreshOutcome::Succeeded
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RetryScheduled(view)
                if view.ordinal == 2
                    && view.cause == ErrorClass::Auth(AuthError::Expired)
                    && view.delay_ms == 0
        )));
    }

    #[tokio::test]
    async fn persistent_401_refreshes_once_then_requires_reconnection() {
        let unauthorized = || {
            MockTurn::Err(ProviderError::Http {
                status: 401,
                message: "unauthorized".into(),
                retry_after_ms: None,
            })
        };
        let h = harness(
            vec![unauthorized(), unauthorized(), text_turn("must not open")],
            false,
            100_000,
        );
        let refreshes = Arc::clone(&h.refreshes);
        let log = Arc::clone(&h.log);
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;

        assert_eq!(*refreshes.lock().unwrap(), 1);
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            2,
            "the persistent 401 must not open a third provider attempt"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Auth(
                AuthError::ReconnectRequired
            )))
        ));
    }

    #[tokio::test]
    async fn cancellation_during_refresh_starts_no_provider_retry() {
        let h = harness(
            vec![MockTurn::Err(ProviderError::Http {
                status: 401,
                message: "unauthorized".into(),
                retry_after_ms: None,
            })],
            false,
            100_000,
        );
        *h.refresh_behavior.lock().unwrap() = RefreshBehavior::Block;
        let started = Arc::clone(&h.refresh_started);
        let log = Arc::clone(&h.log);
        let cancel = h.deps.cancel.clone();
        let task = tokio::spawn(drive(
            AgentContext::new("mock").push(Message::user("go")),
            h.deps,
        ));

        started.acquire().await.unwrap().forget();
        cancel.cancel();
        let events = tokio::time::timeout(std::time::Duration::from_millis(100), task)
            .await
            .expect("refresh cancellation must return within 100 ms")
            .unwrap();

        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CredentialRefresh(view)
                if view.outcome == crate::CredentialRefreshOutcome::Cancelled
        )));
        assert!(matches!(events.last(), Some(AgentEvent::Interrupted)));
    }

    #[tokio::test]
    async fn cancellation_during_backoff_starts_no_provider_retry() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Transport("temporary cut".into())),
                text_turn("must not open"),
            ],
            false,
            100_000,
        );
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let log = Arc::clone(&h.log);
        let cancel = h.deps.cancel.clone();
        let mut deps = h.deps;
        deps.clock = Arc::new(BlockingClock {
            started: Arc::clone(&started),
        });
        let task = tokio::spawn(drive(
            AgentContext::new("mock").push(Message::user("go")),
            deps,
        ));

        started.acquire().await.unwrap().forget();
        cancel.cancel();
        let events = tokio::time::timeout(std::time::Duration::from_millis(100), task)
            .await
            .expect("backoff cancellation must return within 100 ms")
            .unwrap();

        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::RetryScheduled(_)))
        );
        assert!(matches!(events.last(), Some(AgentEvent::Interrupted)));
    }

    #[tokio::test]
    async fn rejected_refresh_requires_reconnection_without_exposing_provider_body() {
        let h = harness(
            vec![MockTurn::Err(ProviderError::Http {
                status: 401,
                message: "unauthorized".into(),
                retry_after_ms: None,
            })],
            false,
            100_000,
        );
        *h.refresh_behavior.lock().unwrap() = RefreshBehavior::Reject;
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
        let encoded = serde_json::to_string(&events).unwrap();

        assert!(!encoded.contains("refresh rejected"));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CredentialRefresh(view)
                if view.outcome == crate::CredentialRefreshOutcome::Rejected
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Auth(
                AuthError::ReconnectRequired
            )))
        ));
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
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RetryScheduled(view)
                if view.ordinal == 2
                    && view.fallback_model.as_deref() == Some("fallback")
        )));
    }

    #[tokio::test]
    async fn fallback_keeps_the_original_attempt_budget_and_terminal_taxonomy() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Http {
                    status: 529,
                    message: "overloaded".into(),
                    retry_after_ms: None,
                }),
                MockTurn::Err(ProviderError::Transport("fallback unavailable".into())),
                text_turn("must not open"),
            ],
            false,
            100_000,
        );
        let requests = Arc::clone(&h.request_models);
        let events = drive(
            AgentContext::new("primary")
                .with_config(RunConfig {
                    max_retries: 1,
                    overload_fallback_model: Some("fallback".into()),
                    ..RunConfig::default()
                })
                .push(Message::user("go")),
            h.deps,
        )
        .await;

        assert_eq!(
            *requests.lock().unwrap(),
            vec!["primary".to_string(), "fallback".to_string()]
        );
        let retries: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::RetryScheduled(view) => Some(view),
                _ => None,
            })
            .collect();
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].ordinal, 2);
        assert_eq!(retries[0].max_attempts, 2);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Provider(failure)))
                if failure.class == Some(ErrorClass::Retryable)
        ));
    }

    #[tokio::test]
    async fn context_recovery_cannot_exceed_the_total_attempt_budget() {
        let h = harness(
            vec![
                MockTurn::Err(ProviderError::Transport("temporary cut".into())),
                MockTurn::Err(ProviderError::ContextLengthExceeded),
                text_turn("must not open"),
            ],
            false,
            100_000,
        );
        let requests = Arc::clone(&h.request_models);
        let events = drive(
            AgentContext::new("model")
                .with_config(RunConfig {
                    max_retries: 1,
                    ..RunConfig::default()
                })
                .push(Message::user("go")),
            h.deps,
        )
        .await;

        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RetryScheduled(_)))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Provider(failure)))
                if failure.class == Some(ErrorClass::ContextLimit)
        ));
    }

    #[tokio::test]
    async fn terminal_provider_details_are_redacted_at_the_public_event_boundary() {
        let secret = "Bearer AT_SECRET account_id=acct_secret";
        let h = harness(
            vec![MockTurn::Err(ProviderError::Stream(secret.into()))],
            false,
            100_000,
        );
        let events = drive(
            AgentContext::new("model")
                .with_config(RunConfig {
                    max_retries: 0,
                    ..RunConfig::default()
                })
                .push(Message::user("go")),
            h.deps,
        )
        .await;
        let encoded = serde_json::to_string(&events).unwrap();

        assert!(!encoded.contains("AT_SECRET"));
        assert!(!encoded.contains("acct_secret"));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(crate::AgentError::Provider(failure)))
                if failure.class == Some(ErrorClass::Retryable)
                    && failure.message == "provider stream failed"
        ));
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
            .push(Message::user("very long history"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("x".repeat(3000)));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Auto),
            "small fallback window should trigger autocompaction: {events:?}"
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
                        text: "ghost ".into(),
                    }],
                    ProviderError::Stream("reset".into()),
                ),
                text_turn("final"),
            ],
            false,
            100_000,
        );
        let mut ctx = AgentContext::new("mock").push(Message::user("go"));
        ctx.turn_id = Some("turn_retry".into());
        let events = drive(ctx, h.deps).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::StreamReset))
                .count(),
            1
        );
        let retry = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::RetryScheduled(view) => Some(view),
                _ => None,
            })
            .expect("retry event");
        assert_eq!(retry.turn_id.as_deref(), Some("turn_retry"));
        assert_eq!(retry.step, 1);
        assert_eq!(retry.ordinal, 2);
        assert_eq!(retry.cause, ErrorClass::Retryable);
        assert_eq!(retry.prompt_fingerprint.len(), 64);
        assert_eq!(retry.model_runtime_fingerprint.len(), 64);
        assert_eq!(retry.tool_plan_fingerprint.len(), 64);
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    }

    #[tokio::test]
    async fn tool_output_deltas_do_not_change_headless_text() {
        // US-015 AC5: headless mode ignores fragments, its textual output stays
        // byte-for-byte identical to the one produced without streaming.
        let turns = || vec![tool_turn("t1"), text_turn("resultat final")];
        let plain = harness(turns(), false, 100_000);
        let plain_res = run_headless(
            with_mock_tool(AgentContext::new("mock").push(Message::user("go"))),
            plain.deps,
        )
        .await;

        let mut streamed = harness(turns(), false, 100_000);
        streamed.deps.tools = Arc::new(StreamingTools);
        let streamed_res = run_headless(
            with_mock_tool(AgentContext::new("mock").push(Message::user("go"))),
            streamed.deps,
        )
        .await;

        assert_eq!(streamed_res.text, plain_res.text);
        assert_eq!(streamed_res.text, "resultat final");
        // The fragments do exist in the event flow, they are simply not
        // rendered by text mode.
        assert!(streamed_res.events > plain_res.events);
    }

    #[tokio::test]
    async fn maxtokens_plain_text_is_exhausted_not_success() {
        let h = harness(
            vec![MockTurn::Stream(vec![
                StreamEvent::TextDelta {
                    text: "truncated".into(),
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
            "missing outcome should break the contract: {events:?}"
        );
    }

    // US-006/008: MaxTokens in the middle of a tool_call -> withholding (Recover)
    // -> reactive, the tool intent is not silently dropped.
    #[tokio::test]
    async fn maxtokens_midtool_recovers_in_loop() {
        let h = harness(
            vec![
                MockTurn::Stream(vec![
                    StreamEvent::tool_call_start("c1", "bash"),
                    StreamEvent::ToolCallDelta {
                        id: "c1".into(),
                        input_delta: "{\"cm".into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::MaxTokens,
                    },
                ]),
                text_turn("regenerated"),
            ],
            false,
            100_000,
        );
        let ctx = AgentContext::new("mock")
            .push(Message::user("initial context"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("do X"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "reactive expected: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t.contains("regenerated")))
        );
    }

    // US-008 AC1: microcompaction triggered INSIDE the loop (micro threshold 70%,
    // below the auto 80%) -> Compacted(Micro), without Auto.
    #[tokio::test]
    async fn microcompaction_triggers_in_loop_below_auto() {
        // window 1000, reserve 200 -> micro 560, auto 640. usage=600 ∈ [560,640).
        let turn = MockTurn::Stream(vec![
            StreamEvent::Usage {
                usage: TokenUsage {
                    input: 600,
                    output: 5,
                },
            },
            StreamEvent::tool_call_start("c1", "bash"),
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                input_delta: "{}".into(),
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
            "microcompaction expected: {events:?}"
        );
        assert!(
            !has_compacted(&events, CompactKind::Auto),
            "pas d'auto sous le seuil: {events:?}"
        );
    }

    // ───────── US-002 / US-003: consumption and quota in the contract ─────────

    fn model_turns(events: &[AgentEvent]) -> Vec<crate::ModelTurnView> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ModelTurn(view) => Some(*view),
                _ => None,
            })
            .collect()
    }

    fn text_turn_with(events: Vec<StreamEvent>) -> MockTurn {
        let mut all = events;
        all.push(StreamEvent::TextDelta {
            text: "réponse".into(),
        });
        all.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
        });
        MockTurn::Stream(all)
    }

    /// US-002 AC1: the end-of-round-trip event carries the real occupancy of the
    /// window and the window of the active model, next to the counters already
    /// present.
    #[tokio::test]
    async fn model_turn_carries_backend_usage_and_model_window() {
        let h = harness(
            vec![text_turn_with(vec![StreamEvent::Usage {
                usage: TokenUsage {
                    input: 600,
                    output: 5,
                },
            }])],
            false,
            10_000,
        );
        let events = drive(
            AgentContext::new("windowed").push(Message::user("go")),
            h.deps,
        )
        .await;
        let turns = model_turns(&events);
        assert_eq!(turns.len(), 1, "{events:?}");
        assert_eq!(turns[0].context_tokens, Some(600));
        assert_eq!(turns[0].context_window, Some(2_000));
        assert_eq!(
            turns[0].estimated_context_tokens, None,
            "sonde inactive par défaut"
        );
    }

    /// US-002 AC3 and AC4: without a reported usage the measure is declared
    /// absent instead of being reported as zero, and an unknown window stays
    /// `None` so that no percentage can be computed in the core.
    #[tokio::test]
    async fn model_turn_reports_absent_measure_and_unknown_window() {
        let h = harness(vec![text_turn_with(Vec::new())], false, 10_000);
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
        let turns = model_turns(&events);
        assert_eq!(turns.len(), 1, "{events:?}");
        assert_eq!(
            turns[0].context_tokens, None,
            "mesure absente, jamais rapportée à zéro"
        );
        assert_eq!(turns[0].context_window, None, "fenêtre inconnue");
        assert!(
            turns[0].input_tokens > 0,
            "les compteurs cumulés gardent leur repli estimé"
        );
    }

    /// US-002 AC5: the calibration probe is now data carried by the event; the
    /// core computes it on demand and writes nothing.
    #[tokio::test]
    async fn usage_probe_travels_as_data_when_enabled() {
        let h = harness(
            vec![text_turn_with(vec![StreamEvent::Usage {
                usage: TokenUsage {
                    input: 600,
                    output: 5,
                },
            }])],
            false,
            10_000,
        );
        let ctx = AgentContext::new("windowed")
            .with_config(RunConfig {
                usage_probe: true,
                ..RunConfig::default()
            })
            .push(Message::user("go"));
        let turns = model_turns(&drive(ctx, h.deps).await);
        assert!(
            turns[0].estimated_context_tokens.is_some(),
            "sonde active: l'estimation locale accompagne la mesure"
        );
    }

    /// US-003 AC1: a quota state served by the provider becomes a structured
    /// event; an empty state produces nothing at all (AC5).
    #[tokio::test]
    async fn quota_state_is_relayed_and_emptiness_is_silent() {
        let snapshot = crate::quota::QuotaSnapshot {
            primary: Some(crate::quota::QuotaWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at_unix: Some(1_784_989_920),
            }),
            secondary: None,
        };
        let h = harness(
            vec![text_turn_with(vec![StreamEvent::Quota { snapshot }])],
            false,
            10_000,
        );
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
        let quotas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Quota(snapshot) => Some(*snapshot),
                _ => None,
            })
            .collect();
        assert_eq!(quotas.len(), 1, "{events:?}");
        assert_eq!(quotas[0].primary.map(|w| w.used_percent), Some(42.0));

        let h = harness(
            vec![text_turn_with(vec![StreamEvent::Quota {
                snapshot: crate::quota::QuotaSnapshot::default(),
            }])],
            false,
            10_000,
        );
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Quota(_))),
            "état vide: aucun événement émis ({events:?})"
        );
    }

    // ───────── US-014: loop guardrails + budgets (kill-switch) ─────────

    use crate::transition::ExhaustReason;

    /// Tool turn emitting an explicit `usage` (to drive the budget).
    fn tool_turn_usage(id: &str, input: u32, output: u32) -> MockTurn {
        MockTurn::Stream(vec![
            StreamEvent::Usage {
                usage: TokenUsage { input, output },
            },
            StreamEvent::tool_call_start(id, "bash"),
            StreamEvent::ToolCallDelta {
                id: id.into(),
                input_delta: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ])
    }

    // US-014 AC1: same tool + same args repeated -> explicit signal to the agent
    // (batch not executed), then deterministic stop if the loop persists.
    #[tokio::test]
    async fn loop_guardrail_signals_then_aborts() {
        // The model keeps asking for the same `bash {cmd:ls}` forever.
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

        // Explicit signal sent back to the agent (edge case #2).
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult(v) if v.content.contains("Loop detected") && v.is_error
            )),
            "explicit loop signal expected: {events:?}"
        );
        // Deterministic stop beyond the signal.
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::ToolLoop { .. }))
            ),
            "ToolLoop ending expected: {events:?}"
        );
    }

    // US-014: a DIFFERENT batch on every turn does not trip the guardrail.
    #[tokio::test]
    async fn loop_guardrail_does_not_false_positive_on_distinct_calls() {
        let distinct = |id: &str, cmd: &str| {
            MockTurn::Stream(vec![
                StreamEvent::tool_call_start(id, "bash"),
                StreamEvent::ToolCallDelta {
                    id: id.into(),
                    input_delta: format!("{{\"cmd\":\"{cmd}\"}}"),
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
        let ctx = AgentContext::new("mock").push(Message::user("three distinct actions"));
        let events = drive(ctx, h.deps).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Exhausted(ExhaustReason::ToolLoop { .. }))),
            "no loop should be detected: {events:?}"
        );
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    }

    // US-014 AC2: cumulated token budget reached -> kill-switch (edge case #3).
    #[tokio::test]
    async fn token_budget_kill_switch_stops_run() {
        // Turn 1 consumes 150 tokens (>120); turn 2 must never start.
        let h = harness(
            vec![tool_turn_usage("c1", 100, 50), text_turn("never reached")],
            false,
            1_000_000,
        );
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(120),
                max_output_tokens: 10, // small, so the pre-turn estimate does not stop turn 1
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
            "budget kill-switch expected: {events:?}"
        );
        // The 1st tool turn did happen before the kill-switch.
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
                text_turn("never reached"),
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
            .push(Message::user("context"))
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
            "failed stream usage should count: {events:?}"
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1,
            "retry should be blocked by the budget before reopening a stream"
        );
    }

    #[tokio::test]
    async fn compaction_usage_counts_against_token_budget() {
        let h = harness_with_summary_usage(
            vec![
                MockTurn::Err(ProviderError::ContextLengthExceeded),
                text_turn("never reached"),
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
            .push(Message::user("context"))
            .push(Message::assistant_text("ok"))
            .push(Message::user("go"));
        let events = drive(ctx, h.deps).await;
        assert!(
            has_compacted(&events, CompactKind::Reactive),
            "reactive compaction expected: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget {
                    spent: 150,
                    limit: 120
                }))
            ),
            "compaction usage should count: {events:?}"
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| **entry == "stream")
                .count(),
            1,
            "no post-compaction stream should start after the budget is reached"
        );
    }

    // US-014 AC3: PRE-turn estimation -> we stop BEFORE a turn that costs too much
    // (no provider call emitted).
    #[tokio::test]
    async fn pre_turn_estimate_stops_before_expensive_turn() {
        let h = harness(vec![text_turn("never")], false, 1_000_000);
        let ctx = AgentContext::new("mock")
            .with_config(RunConfig {
                token_budget: Some(5), // < max_output -> the projection overshoots right away
                max_output_tokens: 100,
                ..RunConfig::default()
            })
            .push(Message::user("task"));
        let events = drive(ctx, h.deps).await;
        assert!(
            matches!(
                events.first(),
                Some(AgentEvent::Exhausted(ExhaustReason::TokenBudget { .. }))
            ),
            "pre-turn stop expected: {events:?}"
        );
        // No provider stream must have been opened.
        assert!(
            !h.log.lock().unwrap().contains(&"stream"),
            "provider should not be called: {:?}",
            h.log.lock().unwrap()
        );
    }

    // ───────── US-001 / US-002: cooperative cancellation ─────────

    /// Content of the PERSISTED `tool_result`s, in order. `Message::text()` only
    /// reads `Text` blocks and would say nothing about a tool result.
    fn persisted_tool_results(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_results(events: &[AgentEvent]) -> Vec<&crate::event::ToolResultView> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    // US-001 AC4: signal already set on entry -> stop at the first boundary,
    // `Interrupted` emitted by the CORE, no provider call.
    #[tokio::test]
    async fn cancel_before_start_stops_at_the_first_boundary() {
        let h = harness(vec![text_turn("jamais")], false, 100_000);
        let mut deps = h.deps.clone();
        let cancel = CancellationToken::new();
        cancel.cancel();
        deps.cancel = cancel;
        let events = drive(AgentContext::new("mock").push(Message::user("hello")), deps).await;
        assert!(
            matches!(events.as_slice(), [AgentEvent::Interrupted]),
            "single Interrupted expected: {events:?}"
        );
        assert!(
            !h.log.lock().unwrap().contains(&"stream"),
            "provider should not be called: {:?}",
            h.log.lock().unwrap()
        );
    }

    // US-001 AC2: cancellation during streaming -> no `Text` after the
    // boundary; whatever already scrolled by stays in the persisted transcript.
    #[tokio::test]
    async fn cancel_during_stream_stops_emitting_deltas() {
        let cancel = CancellationToken::new();
        let h = harness(
            vec![MockTurn::StreamCancelling(
                vec![
                    StreamEvent::TextDelta {
                        text: "premier".into(),
                    },
                    StreamEvent::TextDelta {
                        text: "second".into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::EndTurn,
                    },
                ],
                cancel.clone(),
            )],
            false,
            100_000,
        );
        let mut deps = h.deps.clone();
        deps.cancel = cancel;
        let events = drive(AgentContext::new("mock").push(Message::user("hi")), deps).await;
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["premier"], "stream drained after cancel");
        assert!(
            matches!(events.last(), Some(AgentEvent::Interrupted)),
            "{events:?}"
        );
        let synced = h.boundaries.synced.lock().unwrap().clone();
        assert!(
            synced.iter().any(|m| m.text().contains("premier")),
            "partial answer kept: {synced:?}"
        );
    }

    // US-001 AC3 + US-002 AC1/AC3: interruption during a dispatch that never
    // hands back control -> the loop takes control back, writes a synthetic result
    // per in-flight call, THEN persists.
    #[tokio::test]
    async fn interrupted_dispatch_writes_synthetic_results_before_persisting() {
        let cancel = CancellationToken::new();
        let h = harness(vec![tool_turn("c1")], false, 100_000);
        let mut deps = h.deps.clone();
        deps.cancel = cancel.clone();
        deps.tools = Arc::new(HangingTools(cancel));
        let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

        assert!(
            matches!(events.last(), Some(AgentEvent::Interrupted)),
            "{events:?}"
        );
        let results = tool_results(&events);
        assert_eq!(results.len(), 1, "{events:?}");
        assert_eq!(results[0].id, "c1");
        assert!(results[0].is_error, "interrupted result is an error");
        assert_eq!(results[0].content, INTERRUPTED_TOOL_RESULT);

        let synced = h.boundaries.synced.lock().unwrap().clone();
        assert!(
            unanswered_tool_calls(&synced).is_empty(),
            "no orphan call in the persisted transcript: {synced:?}"
        );
        assert_eq!(
            persisted_tool_results(&synced),
            vec![INTERRUPTED_TOOL_RESULT.to_string()],
            "the synthetic result is persisted: {synced:?}"
        );
    }

    // US-002 AC5: several concurrent calls -> exactly one result per call,
    // with no duplicate and none forgotten.
    #[tokio::test]
    async fn interrupted_concurrent_dispatch_answers_every_call_once() {
        let cancel = CancellationToken::new();
        let h = harness(vec![tool_turn_n(&["c1", "c2"])], false, 100_000);
        let mut deps = h.deps.clone();
        deps.cancel = cancel.clone();
        deps.tools = Arc::new(HangingTools(cancel));
        let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

        let ids: Vec<&str> = tool_results(&events)
            .iter()
            .map(|v| v.id.as_str())
            .collect();
        assert_eq!(ids, vec!["c1", "c2"], "{events:?}");
        let synced = h.boundaries.synced.lock().unwrap().clone();
        assert!(unanswered_tool_calls(&synced).is_empty(), "{synced:?}");
        assert_eq!(
            persisted_tool_results(&synced),
            vec![
                INTERRUPTED_TOOL_RESULT.to_string(),
                INTERRUPTED_TOOL_RESULT.to_string()
            ],
            "exactly one result per call: {synced:?}"
        );
    }

    // Edge case #2: the dispatch completes in the same window as the cancellation
    // -> the REAL result is kept, no synthetic result overwrites it.
    #[tokio::test]
    async fn tool_finished_before_stop_keeps_its_real_result() {
        let cancel = CancellationToken::new();
        let h = harness(vec![tool_turn("c1")], false, 100_000);
        let mut deps = h.deps.clone();
        deps.cancel = cancel.clone();
        deps.tools = Arc::new(RacingTools(cancel));
        let events = drive(AgentContext::new("mock").push(Message::user("ls")), deps).await;

        let results = tool_results(&events);
        assert_eq!(results.len(), 1, "{events:?}");
        assert_eq!(results[0].content, "real output");
        assert!(!results[0].is_error);
        let synced = h.boundaries.synced.lock().unwrap().clone();
        assert_eq!(
            persisted_tool_results(&synced),
            vec!["real output".to_string()],
            "no synthetic result over a real one: {synced:?}"
        );
        assert!(matches!(events.last(), Some(AgentEvent::Interrupted)));
    }

    // PRD reliability metric: 0 corrupted sessions out of 50 interruptions
    // during a tool dispatch. CORE version: the cancellation point, the number
    // of concurrent calls and whether the tools terminate vary on every
    // iteration. Replaying the same sweep at CLI level belongs to US-007.
    #[tokio::test]
    async fn fifty_interruptions_during_dispatch_never_corrupt_the_transcript() {
        const IDS: [&str; 3] = ["c1", "c2", "c3"];
        for run in 0..50usize {
            let ids = &IDS[..=(run % 3)];
            let cancel = CancellationToken::new();
            let h = harness(vec![tool_turn_n(ids)], false, 100_000);
            let mut deps = h.deps.clone();
            deps.cancel = cancel.clone();
            deps.tools = Arc::new(DelayedCancelTools {
                cancel,
                yields: run % 4,
                finish: run % 2 == 0,
            });
            let events = drive(AgentContext::new("mock").push(Message::user("go")), deps).await;

            assert!(
                matches!(events.last(), Some(AgentEvent::Interrupted)),
                "run {run}: the loop must stop on its own: {events:?}"
            );
            let synced = h.boundaries.synced.lock().unwrap().clone();
            assert!(
                unanswered_tool_calls(&synced).is_empty(),
                "run {run}: orphan call persisted: {synced:?}"
            );
            assert_eq!(
                persisted_tool_results(&synced).len(),
                ids.len(),
                "run {run}: exactly one result per call: {synced:?}"
            );
        }
    }

    // US-001 AC6: signal received while the loop is already done -> ignored, with
    // no panic and no extra event.
    #[tokio::test]
    async fn cancel_after_completion_is_ignored() {
        let h = harness(vec![text_turn("done")], false, 100_000);
        let mut deps = h.deps.clone();
        let cancel = CancellationToken::new();
        deps.cancel = cancel.clone();
        let events = drive(AgentContext::new("mock").push(Message::user("hi")), deps).await;
        assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
        cancel.cancel();
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Interrupted)),
            "{events:?}"
        );
    }

    // ───────── EP-003: what a tool sends back besides its text ─────────

    /// Dispatcher that produces the two structured side channels EP-003 adds:
    /// an image in the outcome (US-011) and a plan as an event (US-009).
    struct RichTools;
    #[async_trait::async_trait]
    impl ToolDispatch for RichTools {
        async fn dispatch(
            &self,
            calls: Vec<ToolInvocation>,
            events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            events.emit(crate::tools::ToolDispatchEvent::Plan(crate::PlanView {
                explanation: None,
                steps: vec![crate::PlanStep {
                    step: "étape".into(),
                    status: crate::PlanStatus::InProgress,
                }],
            }));
            calls
                .into_iter()
                .map(|c| ToolOutcome {
                    images: vec![crate::tools::ToolImage {
                        media_type: "image/png".into(),
                        data: "QUJD".into(),
                    }],
                    ..ToolOutcome::new(c.id, "read".into(), false, true, None)
                })
                .collect()
        }
    }

    /// US-011 AC1: an image read by a tool enters the transcript as an image
    /// block and is therefore sent to the provider on the next round-trip.
    /// US-009 AC3: the plan reaches the client as an `AgentEvent`.
    #[tokio::test]
    async fn tool_images_reach_the_provider_and_the_plan_reaches_the_client() {
        let h = harness(vec![tool_turn("call-1"), text_turn("done")], false, 100_000);
        let mut deps = h.deps.clone();
        deps.tools = Arc::new(RichTools);
        let events = drive(AgentContext::new("mock").push(Message::user("look")), deps).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Plan(view) if view.steps.len() == 1)),
            "the plan must be surfaced to the client: {events:?}"
        );

        let reqs = h.requests.lock().unwrap();
        let second = reqs.get(1).expect("a second round-trip is expected");
        let image = second
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::Image { media_type, data } => Some((media_type, data)),
                _ => None,
            })
            .expect("the image must have entered the transcript");
        assert_eq!(image.0, "image/png");
        assert_eq!(image.1, "QUJD");
        assert!(
            second
                .iter()
                .any(|m| matches!(m.role, crate::Role::User) && m.has_images()),
            "the image travels as a user message, next to the textual result"
        );
    }
}
