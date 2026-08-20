//! Test doubles shared by the `run_agent` acceptance tests.
//!
//! One provider, one session, two clocks and a handful of tool dispatchers.
//! Every loop test observes the SAME engine through the SAME seam, so a
//! difference between two tests is a difference in behavior, never in fixtures.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::Deps;
use agent_core::clock::Clock;
use agent_core::compaction::CompactKind;
use agent_core::message::ContentBlock;
use agent_core::message::Message;
use agent_core::provider::AuthError;
use agent_core::provider::CanonicalRequest;
use agent_core::provider::CanonicalResponse;
use agent_core::provider::Capabilities;
use agent_core::provider::ErrorClass;
use agent_core::provider::Provider;
use agent_core::provider::ProviderError;
use agent_core::provider::ProviderKind;
use agent_core::provider::StopReason;
use agent_core::provider::StreamEvent;
use agent_core::provider::TokenUsage;
use agent_core::provider::ToolSpec;
use agent_core::run_agent;
use agent_core::session::Session;
use agent_core::session::SessionError;
use agent_core::tools::ModelToolResult;
use agent_core::tools::ToolDispatch;
use agent_core::tools::ToolEventSink;
use agent_core::tools::ToolInvocation;
use agent_tokenizer::HeuristicCounter;
use futures_util::StreamExt;
use futures_util::pin_mut;
use futures_util::stream::BoxStream;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

// ───────── test doubles (injected through Deps) ─────────

pub enum MockTurn {
    Stream(Vec<StreamEvent>),
    /// Error when OPENING the stream.
    Err(ProviderError),
    /// A few events THEN an error MID-stream.
    StreamThenErr(Vec<StreamEvent>, ProviderError),
    /// US-001: the cancel signal fires while the stream is being consumed,
    /// exactly after the first delivered event.
    StreamCancelling(Vec<StreamEvent>, agent_core::CancellationToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshBehavior {
    Succeed,
    Reject,
    Auth(AuthError),
    Block,
}

pub struct MockProvider {
    pub caps: Capabilities,
    pub turns: Mutex<VecDeque<MockTurn>>,
    pub summary: String,
    pub summary_usage: TokenUsage,
    pub summary_fails: bool,
    pub log: Arc<Mutex<Vec<&'static str>>>,
    pub refreshes: Arc<Mutex<u32>>,
    pub refresh_behavior: Arc<Mutex<RefreshBehavior>>,
    pub refresh_started: Arc<tokio::sync::Semaphore>,
    /// Captures the `messages` of every request (US-028: check the ephemeral
    /// injection without touching the persisted transcript).
    pub requests: Arc<Mutex<Vec<Vec<Message>>>>,
    pub request_models: Arc<Mutex<Vec<String>>>,
    pub request_replays: Arc<Mutex<Vec<bool>>>,
    pub complete_models: Arc<Mutex<Vec<String>>>,
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
    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
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
            RefreshBehavior::Auth(error) => Err(ProviderError::Credential(error)),
            RefreshBehavior::Block => {
                self.refresh_started.add_permits(1);
                std::future::pending().await
            }
        }
    }
}

pub struct InMemorySession {
    pub synced: Mutex<Vec<Message>>,
    pub cursor: Mutex<usize>,
    pub boundaries: Mutex<Vec<CompactKind>>,
    pub log: Arc<Mutex<Vec<&'static str>>>,
    pub fail_context_transition: Mutex<bool>,
    pub baseline: Mutex<Option<agent_core::ContextBaseline>>,
    pub context_transitions: Mutex<Vec<agent_core::ContextTransition>>,
}

#[async_trait::async_trait]
impl Session for InMemorySession {
    fn context_baseline(&self) -> Option<agent_core::ContextBaseline> {
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
        transition: agent_core::ContextTransition,
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
        _snapshot: agent_core::session::FileSnapshot,
    ) -> Result<(), SessionError> {
        Ok(())
    }
}

pub struct NoopClock;

#[async_trait::async_trait]
impl Clock for NoopClock {
    fn now_ms(&self) -> u64 {
        0
    }
    async fn sleep(&self, _dur: std::time::Duration) {}
}

pub struct BlockingClock {
    pub started: Arc<tokio::sync::Semaphore>,
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

pub struct EchoTools;

#[async_trait::async_trait]
impl ToolDispatch for EchoTools {
    /// Stands in for the real registry, which asks the tool itself: an `exec`
    /// cell and the `wait` that polls it are orchestration and are MEANT to be
    /// submitted again unchanged.
    fn loop_guard_exempt(&self, call: &ToolInvocation) -> bool {
        call.name == "exec" || call.name == "wait"
    }

    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        calls
            .into_iter()
            .map(|c| ModelToolResult {
                images: Vec::new(),
                ..ModelToolResult::new(c.id, format!("echo:{}", c.input), false, true, None)
            })
            .collect()
    }
}

/// US-015: tools that publish output fragments before their result.
pub struct StreamingTools;

#[async_trait::async_trait]
impl ToolDispatch for StreamingTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        calls
            .into_iter()
            .map(|c| {
                events.emit(agent_core::tools::ToolDispatchEvent::OutputDelta {
                    id: c.id.clone(),
                    stream: agent_core::event::OutputStream::Stdout,
                    chunk: b"progression...\n".to_vec(),
                });
                ModelToolResult {
                    images: Vec::new(),
                    ..ModelToolResult::new(c.id, format!("echo:{}", c.input), false, true, None)
                }
            })
            .collect()
    }
}

pub struct MissingTools;

#[async_trait::async_trait]
impl ToolDispatch for MissingTools {
    async fn dispatch(
        &self,
        _calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        Vec::new()
    }
}

/// US-001: tools that signal cancellation then NEVER hand back control:
/// the loop must take control back without waiting for the dispatch.
pub struct HangingTools(pub agent_core::CancellationToken);

#[async_trait::async_trait]
impl ToolDispatch for HangingTools {
    async fn dispatch(
        &self,
        _calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        self.0.cancel();
        std::future::pending().await
    }
}

/// Edge case #2: cancellation lands in the window between the end of the
/// dispatch and persistence: the REAL results must survive.
pub struct RacingTools(pub agent_core::CancellationToken);

#[async_trait::async_trait]
impl ToolDispatch for RacingTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        self.0.cancel();
        calls
            .into_iter()
            .map(|c| ModelToolResult {
                images: Vec::new(),
                ..ModelToolResult::new(c.id, "real output".into(), false, true, None)
            })
            .collect()
    }
}

/// Sweeps the cancellation window around a dispatch: `yields` yield points
/// before the signal, then termination (real results) or blocking
/// (abandoned calls).
pub struct DelayedCancelTools {
    pub cancel: agent_core::CancellationToken,
    pub yields: usize,
    pub finish: bool,
}

#[async_trait::async_trait]
impl ToolDispatch for DelayedCancelTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        for _ in 0..self.yields {
            tokio::task::yield_now().await;
        }
        self.cancel.cancel();
        if !self.finish {
            std::future::pending::<()>().await;
        }
        calls
            .into_iter()
            .map(|c| ModelToolResult {
                images: Vec::new(),
                ..ModelToolResult::new(c.id, "real output".into(), false, true, None)
            })
            .collect()
    }
}

// ───────── harness ─────────

pub struct Harness {
    pub log: Arc<Mutex<Vec<&'static str>>>,
    pub refreshes: Arc<Mutex<u32>>,
    pub refresh_behavior: Arc<Mutex<RefreshBehavior>>,
    pub refresh_started: Arc<tokio::sync::Semaphore>,
    pub boundaries: Arc<InMemorySession>,
    pub requests: Arc<Mutex<Vec<Vec<Message>>>>,
    pub request_models: Arc<Mutex<Vec<String>>>,
    pub request_replays: Arc<Mutex<Vec<bool>>>,
    pub complete_models: Arc<Mutex<Vec<String>>>,
    pub deps: Deps,
}

pub fn harness(turns: Vec<MockTurn>, summary_fails: bool, max_context: u32) -> Harness {
    harness_with_summary_usage(turns, summary_fails, max_context, TokenUsage::default())
}

pub fn harness_with_summary_usage(
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
            tool_calling: agent_core::provider::ToolCallingCapabilities {
                parallel_tool_calls: true,
                strict_json_schema: false,
                freeform_tools: false,
                ..agent_core::provider::ToolCallingCapabilities::default()
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
        cancel: agent_core::CancellationToken::new(),
        context_window: Default::default(),
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

pub fn with_mock_tool(mut ctx: AgentContext) -> AgentContext {
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

pub async fn drive(ctx: AgentContext, deps: Deps) -> Vec<AgentEvent> {
    let stream = run_agent(with_mock_tool(ctx), deps);
    pin_mut!(stream);
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        out.push(ev);
    }
    out
}

pub fn tool_turn(id: &str) -> MockTurn {
    named_tool_turn(id, "bash", "{\"cmd\":\"ls\"}")
}

pub fn named_tool_turn(id: &str, name: &str, input_delta: &str) -> MockTurn {
    MockTurn::Stream(vec![
        StreamEvent::tool_call_start(id, name),
        StreamEvent::ToolCallDelta {
            id: id.into(),
            input_delta: input_delta.into(),
        },
        StreamEvent::ToolCallEnd { id: id.into() },
        StreamEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

pub fn freeform_tool_turn(id: &str, name: &str, input_delta: &str) -> MockTurn {
    MockTurn::Stream(vec![
        StreamEvent::custom_tool_call_start(id, name),
        StreamEvent::ToolCallDelta {
            id: id.into(),
            input_delta: input_delta.into(),
        },
        StreamEvent::ToolCallEnd { id: id.into() },
        StreamEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

pub fn tool_turn_n(ids: &[&str]) -> MockTurn {
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

pub fn text_turn(t: &str) -> MockTurn {
    MockTurn::Stream(vec![
        StreamEvent::TextDelta { text: t.into() },
        StreamEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

pub fn resolved_runtime(
    slug: &str,
    fingerprint: char,
    comp_hash: &str,
    replay: agent_core::model::ReasoningReplaySupport,
) -> agent_core::model::ResolvedModelRuntime {
    agent_core::model::ResolvedModelRuntime {
        slug: slug.into(),
        source: agent_core::model::ModelRuntimeSource::Embedded {
            version: "test".into(),
        },
        instructions: format!("{slug} instructions"),
        fingerprint: fingerprint.to_string().repeat(64),
        context_window: 100_000,
        auto_compact_token_limit: 90_000,
        input_modalities: vec![agent_core::model::InputModality::Text],
        reasoning_effort: Some("medium".into()),
        supports_verbosity: true,
        verbosity: Some("low".into()),
        supports_parallel_tool_calls: false,
        tool_capabilities: agent_core::model::ModelToolCapabilities::default(),
        service_tiers: Vec::new(),
        reasoning_replay: replay,
        responses_dialect: agent_core::model::ResponsesDialect::Standard,
        tool_mode: agent_core::model::ModelToolMode::Direct,
        multi_agent_version: agent_core::model::MultiAgentVersion::Disabled,
        truncation: agent_core::model::TruncationPolicy {
            mode: agent_core::model::TruncationMode::Tokens,
            limit: 1_000,
        },
        retry: agent_core::model::ModelRetryPolicy {
            max_attempts: 2,
            backoff_base_ms: 1,
        },
        max_output_tokens: 1_024,
        comp_hash: Some(comp_hash.into()),
    }
}

pub fn baseline(runtime: &agent_core::model::ResolvedModelRuntime) -> agent_core::ContextBaseline {
    agent_core::ContextBaseline {
        profile_fingerprint: runtime.fingerprint.clone(),
        model_slug: runtime.slug.clone(),
        comp_hash: runtime.comp_hash.clone(),
        instructions_fingerprint: "1".repeat(64),
        project_context_fingerprint: "2".repeat(64),
        skills_fingerprint: "3".repeat(64),
        tool_plan_fingerprint: "4".repeat(64),
    }
}

pub fn has_compacted(events: &[AgentEvent], kind: CompactKind) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compacted(k) if *k == kind))
}

// ───────── US-002 / US-003: consumption and quota in the contract ─────────

pub fn model_turns(events: &[AgentEvent]) -> Vec<agent_core::ModelTurnView> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelTurn(view) => Some(*view),
            _ => None,
        })
        .collect()
}

pub fn text_turn_with(events: Vec<StreamEvent>) -> MockTurn {
    let mut all = events;
    all.push(StreamEvent::TextDelta {
        text: "réponse".into(),
    });
    all.push(StreamEvent::Done {
        stop: StopReason::EndTurn,
    });
    MockTurn::Stream(all)
}

/// Tool turn emitting an explicit `usage` (to drive the budget).
pub fn tool_turn_usage(id: &str, input: u32, output: u32) -> MockTurn {
    MockTurn::Stream(vec![
        StreamEvent::Usage {
            usage: TokenUsage::new(u64::from(input), u64::from(output)),
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

// ───────── US-001 / US-002: cooperative cancellation ─────────

/// Content of the PERSISTED `tool_result`s, in order. `Message::text()` only
/// reads `Text` blocks and would say nothing about a tool result.
pub fn persisted_tool_results(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

pub fn tool_results(events: &[AgentEvent]) -> Vec<&agent_core::event::ToolResultView> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult(v) => Some(v),
            _ => None,
        })
        .collect()
}

// ───────── EP-003: what a tool sends back besides its text ─────────

/// Dispatcher that produces the two structured side channels EP-003 adds:
/// an image in the outcome (US-011) and a plan as an event (US-009).
pub struct RichTools;

#[async_trait::async_trait]
impl ToolDispatch for RichTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        events.emit(agent_core::tools::ToolDispatchEvent::Plan(
            agent_core::PlanView {
                explanation: None,
                steps: vec![agent_core::PlanStep {
                    step: "étape".into(),
                    status: agent_core::PlanStatus::InProgress,
                }],
            },
        ));
        calls
            .into_iter()
            .map(|c| ModelToolResult {
                images: vec![agent_core::tools::ToolImage {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                }],
                ..ModelToolResult::new(c.id, "read".into(), false, true, None)
            })
            .collect()
    }
}
