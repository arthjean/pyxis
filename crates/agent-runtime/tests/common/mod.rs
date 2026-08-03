//! Fakes shared by the EP-002 acceptance tests.
//!
//! One provider, one session, one clock and a handful of tool dispatchers: every
//! story of the epic observes the SAME engine through the SAME seam, so a
//! difference between two tests is a difference in behavior, never in fixtures.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::clock::Clock;
use agent_core::compaction::CompactKind;
use agent_core::event::AgentEvent;
use agent_core::message::{ContentBlock, Message};
use agent_core::provider::{
    CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderKind, StopReason, StreamEvent, ToolSpec,
};
use agent_core::session::{FileSnapshot, Session, SessionError};
use agent_core::tools::{ModelToolResult, ToolDispatch, ToolEventSink, ToolInvocation};
use agent_core::{
    AgentContext, CancellationToken as CoreCancel, ContextTransition, Deps, RunConfig,
};
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnLimits};
use agent_runtime::id::{RandomIds, ThreadId, TurnId};
use agent_runtime::runner::TurnRequest;
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::thread::{
    MAX_PENDING_INPUTS, RuntimeEvent, ThreadHandle, ThreadOptions, TurnStatus,
};
use agent_tokenizer::HeuristicCounter;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

// ───────── provider ─────────

pub enum Scripted {
    Stream(Vec<StreamEvent>),
    /// Some events THEN a mid-stream failure: the engine is the one that resets
    /// the stream and retries.
    StreamThenErr(Vec<StreamEvent>, ProviderError),
    /// Some events then a stream that never completes. Only a cancellation or a
    /// steer ends it.
    StreamThenHang(Vec<StreamEvent>),
    /// Fails when OPENING the stream.
    OpenErr(ProviderError),
}

type OnOpen = Arc<dyn Fn(usize) + Send + Sync>;

pub struct FakeProvider {
    caps: Capabilities,
    turns: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<CanonicalRequest>>,
    /// Called when a stream is opened, with the 1-based call index. This is the
    /// hook that lets a test move a source WHILE a sampling is in flight.
    on_open: Mutex<Option<OnOpen>>,
    /// When true, every error is fatal instead of retryable: a test about a
    /// failed turn must not wait for three backoffs.
    fatal_errors: bool,
}

impl FakeProvider {
    pub fn new(turns: Vec<Scripted>) -> Arc<Self> {
        Arc::new(Self {
            caps: Capabilities {
                tools: true,
                max_context: 100_000,
                ..Capabilities::default()
            },
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            on_open: Mutex::new(None),
            fatal_errors: false,
        })
    }

    pub fn failing(turns: Vec<Scripted>) -> Arc<Self> {
        let mut provider = Self::new(turns);
        Arc::get_mut(&mut provider).unwrap().fatal_errors = true;
        provider
    }

    pub fn on_open(&self, hook: OnOpen) {
        *self.on_open.lock().unwrap() = Some(hook);
    }

    pub fn requests(&self) -> Vec<CanonicalRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>,
        ProviderError,
    > {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(req);
            requests.len()
        };
        let hook = self.on_open.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(index);
        }
        let scripted = self.turns.lock().unwrap().pop_front();
        match scripted {
            Some(Scripted::Stream(events)) => Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            ))),
            Some(Scripted::StreamThenErr(events, err)) => {
                let mut items: Vec<Result<StreamEvent, ProviderError>> =
                    events.into_iter().map(Ok).collect();
                items.push(Err(err));
                Ok(Box::pin(futures_util::stream::iter(items)))
            }
            Some(Scripted::StreamThenHang(events)) => Ok(Box::pin(
                futures_util::stream::iter(events.into_iter().map(Ok))
                    .chain(futures_util::stream::pending()),
            )),
            Some(Scripted::OpenErr(err)) => Err(err),
            None => Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                },
            )]))),
        }
    }
    async fn complete(&self, _req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        Ok(CanonicalResponse {
            content: vec![ContentBlock::Text {
                text: "résumé".into(),
            }],
            usage: Default::default(),
            stop: StopReason::EndTurn,
        })
    }
    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        if self.fatal_errors {
            return ErrorClass::InvalidRequest;
        }
        match err {
            ProviderError::Http { status: 429, .. } => ErrorClass::RateLimited,
            _ => ErrorClass::Retryable,
        }
    }
}

// ───────── session ─────────

/// Records every transcript the engine persisted, in order.
#[derive(Default)]
pub struct FakeSession {
    syncs: Mutex<Vec<Vec<Message>>>,
}

impl FakeSession {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn last(&self) -> Vec<Message> {
        self.syncs
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
    pub fn syncs(&self) -> usize {
        self.syncs.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Session for FakeSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        self.syncs.lock().unwrap().push(messages.to_vec());
        Ok(())
    }
    async fn checkpoint(
        &self,
        _kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        self.syncs.lock().unwrap().push(messages.to_vec());
        Ok(())
    }
    async fn record_context_transition(
        &self,
        _transition: ContextTransition,
    ) -> Result<(), SessionError> {
        Ok(())
    }
    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
        Ok(())
    }
    async fn record_file_snapshot(&self, _snapshot: FileSnapshot) -> Result<(), SessionError> {
        Ok(())
    }
}

// ───────── tools ─────────

pub struct EchoTools;

#[async_trait::async_trait]
impl ToolDispatch for EchoTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        calls
            .into_iter()
            .map(|call| {
                ModelToolResult::new(
                    call.id,
                    format!("dispatched:{}", call.name),
                    false,
                    true,
                    None,
                )
            })
            .collect()
    }
}

/// Announces that it started, then waits for the test to release it. Used to
/// hold a turn inside its tool phase while another operation is submitted.
pub struct GatedTools {
    pub started: Arc<tokio::sync::Semaphore>,
    pub release: Arc<tokio::sync::Semaphore>,
}

impl GatedTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        })
    }
}

#[async_trait::async_trait]
impl ToolDispatch for GatedTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        self.started.add_permits(1);
        if let Ok(permit) = self.release.acquire().await {
            permit.forget();
        }
        calls
            .into_iter()
            .map(|call| ModelToolResult::new(call.id, "outil terminé".into(), false, true, None))
            .collect()
    }
}

/// Never returns. Dropping its future is the only way out, which is what a
/// cooperative cancellation of the loop does.
pub struct HangingTools;

#[async_trait::async_trait]
impl ToolDispatch for HangingTools {
    async fn dispatch(
        &self,
        _calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        std::future::pending().await
    }
}

// ───────── clock ─────────

pub struct InstantClock;

#[async_trait::async_trait]
impl Clock for InstantClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
    async fn sleep(&self, _dur: Duration) {}
}

// ───────── wiring ─────────

pub fn deps(
    provider: Arc<FakeProvider>,
    session: Arc<FakeSession>,
    tools: Arc<dyn ToolDispatch>,
) -> Deps {
    Deps {
        provider,
        session,
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(InstantClock),
        tools,
        // Overridden per turn by `RunAgentRunner`; a root here would be an orphan
        // branch, which is exactly what US-008 AC1 forbids.
        cancel: CoreCancel::new(),
        context_window: Default::default(),
    }
}

pub fn turn_context(turn_id: TurnId) -> TurnContext {
    TurnContext {
        turn_id,
        model: "test-model".into(),
        reasoning_effort: None,
        model_runtime_fingerprint: None,
        permission_mode: "ask".into(),
        sandbox: "workspace-write".into(),
        workspace: std::path::PathBuf::from("/tmp/pyxis-test"),
        limits: TurnLimits {
            max_turns: 50,
            max_output_tokens: 4096,
            max_pending_inputs: MAX_PENDING_INPUTS,
        },
    }
}

/// Builds the `AgentContext` of a turn: the transcript is the turn's text alone,
/// so a request's messages are exactly what the runtime handed over.
pub fn agent_context(request: &TurnRequest) -> AgentContext {
    let mut context = AgentContext::new(request.context.model.clone())
        .with_config(RunConfig {
            max_retries: 1,
            backoff_base_ms: 0,
            ..RunConfig::default()
        })
        .push(Message::user(request.text.clone()));
    context.tools = ["bash", "bloquant", "echo", "lent", "read"]
        .into_iter()
        .map(|name| {
            ToolSpec::function(
                name,
                "runtime test dispatcher",
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
        })
        .collect();
    context
}

pub struct Harness {
    pub handle: Arc<ThreadHandle>,
    pub thread_id: ThreadId,
    pub store: Arc<MemoryThreadStore>,
    pub contexts: Arc<FixedTurnContext>,
    pub root: CancellationToken,
}

/// Starts a thread over a real `RunAgentRunner`, so every assertion below is
/// made on the production seam and not on a scripted stand-in.
pub async fn start(runner: Arc<dyn agent_runtime::runner::TurnRunner>) -> Harness {
    start_with(runner, None).await
}

/// Same, with the sub-agent supervisor this thread owns (EP-004).
pub async fn start_with(
    runner: Arc<dyn agent_runtime::runner::TurnRunner>,
    agents: Option<Arc<agent_runtime::supervisor::AgentSupervisor>>,
) -> Harness {
    start_on(runner, agents, Arc::new(MemoryThreadStore::new())).await
}

/// Same, on a store a test pre-filled: what a resume replays.
pub async fn start_on(
    runner: Arc<dyn agent_runtime::runner::TurnRunner>,
    agents: Option<Arc<agent_runtime::supervisor::AgentSupervisor>>,
    store: Arc<MemoryThreadStore>,
) -> Harness {
    let thread_id = store
        .read()
        .await
        .expect("the store reads")
        .thread_id
        .unwrap_or_else(|| ThreadId::generate(&RandomIds));
    let contexts = Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
        &RandomIds,
    ))));
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store: Arc::clone(&store) as Arc<dyn ThreadStore>,
        runner,
        turn_contexts: Arc::clone(&contexts) as Arc<dyn agent_runtime::context::TurnContextSource>,
        ids: Arc::new(RandomIds),
        clock: Arc::new(InstantClock),
        parent_cancel: root.clone(),
        agents,
    })
    .await
    .expect("the thread starts");
    Harness {
        handle: Arc::new(handle),
        thread_id,
        store,
        contexts,
        root,
    }
}

// ───────── sub-agents (EP-004) ─────────

/// One child's provider script. `fatal` makes its errors non-retryable, which
/// is how a test gets a child that actually fails instead of one the engine
/// patiently retries.
pub struct ChildScript {
    pub turns: Vec<Scripted>,
    pub fatal: bool,
}

impl ChildScript {
    pub fn new(turns: Vec<Scripted>) -> Self {
        Self {
            turns,
            fatal: false,
        }
    }

    pub fn fatal(turns: Vec<Scripted>) -> Self {
        Self { turns, fatal: true }
    }
}

/// What a test keeps of a spawned child: its store and its provider, so both
/// its durable log and the requests it made are assertable.
type SpawnedChild = (
    agent_runtime::id::AgentId,
    Arc<MemoryThreadStore>,
    Arc<FakeProvider>,
);

/// Builds children out of the same fakes as the parent (US-013).
///
/// One provider script per spawn, handed out in order. The requests it received
/// and the store of each child stay reachable, so a test can assert what a child
/// was asked to do and what its own durable log holds.
pub struct ScriptedSpawner {
    scripts: Mutex<VecDeque<ChildScript>>,
    seen: Mutex<Vec<agent_runtime::supervisor::ChildRequest>>,
    children: Mutex<Vec<SpawnedChild>>,
    /// Observed at the START of each spawn: what the parent had already done
    /// before the child existed at all.
    before_create: Mutex<Vec<usize>>,
    parent_store: Mutex<Option<Arc<MemoryThreadStore>>>,
    /// Refuses every spawn, to exercise the failure path.
    broken: bool,
}

impl ScriptedSpawner {
    pub fn new(scripts: Vec<ChildScript>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            seen: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            before_create: Mutex::new(Vec::new()),
            parent_store: Mutex::new(None),
            broken: false,
        })
    }

    pub fn broken() -> Arc<Self> {
        let mut spawner = Self::new(Vec::new());
        Arc::get_mut(&mut spawner).unwrap().broken = true;
        spawner
    }

    /// Watches the parent's log at the instant each child is created.
    pub fn watch(&self, parent: Arc<MemoryThreadStore>) {
        *self.parent_store.lock().unwrap() = Some(parent);
    }

    pub fn requests(&self) -> Vec<agent_runtime::supervisor::ChildRequest> {
        self.seen.lock().unwrap().clone()
    }

    pub fn store_of(&self, agent_id: agent_runtime::id::AgentId) -> Arc<MemoryThreadStore> {
        self.children
            .lock()
            .unwrap()
            .iter()
            .find(|(id, ..)| *id == agent_id)
            .map(|(_, store, _)| Arc::clone(store))
            .expect("a child store for this agent")
    }

    pub fn provider_of(&self, agent_id: agent_runtime::id::AgentId) -> Arc<FakeProvider> {
        self.children
            .lock()
            .unwrap()
            .iter()
            .find(|(id, ..)| *id == agent_id)
            .map(|(.., provider)| Arc::clone(provider))
            .expect("a child provider for this agent")
    }

    /// Events the parent's log already held when each child was created.
    pub fn parent_events_before_create(&self) -> Vec<usize> {
        self.before_create.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl agent_runtime::supervisor::AgentSpawner for ScriptedSpawner {
    async fn spawn(
        &self,
        request: &agent_runtime::supervisor::ChildRequest,
    ) -> Result<agent_runtime::supervisor::ChildParts, String> {
        if self.broken {
            return Err("store enfant indisponible".into());
        }
        let watched = self.parent_store.lock().unwrap().clone();
        if let Some(parent) = watched {
            let events = parent.read().await.map(|s| s.events.len()).unwrap_or(0);
            self.before_create.lock().unwrap().push(events);
        }
        self.seen.lock().unwrap().push(request.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChildScript::new(Vec::new()));
        let provider = if script.fatal {
            FakeProvider::failing(script.turns)
        } else {
            FakeProvider::new(script.turns)
        };
        let store = Arc::new(MemoryThreadStore::new());
        self.children.lock().unwrap().push((
            request.agent_id,
            Arc::clone(&store),
            Arc::clone(&provider),
        ));
        Ok(agent_runtime::supervisor::ChildParts {
            store: store as Arc<dyn ThreadStore>,
            runner: Arc::new(agent_runtime::runner::RunAgentRunner::new(
                deps(
                    Arc::clone(&provider),
                    FakeSession::new(),
                    Arc::new(EchoTools),
                ),
                agent_context,
            )),
            turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
                &RandomIds,
            )))),
        })
    }
}

/// Collects the live event stream until the channel closes.
pub fn collect_events(
    mut rx: tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Arc<Mutex<Vec<RuntimeEvent>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            sink.lock().unwrap().push(event);
        }
    });
    seen
}

pub async fn wait_for(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(check(), "timed out waiting for {what}");
}

pub async fn wait_for_terminal(handle: &ThreadHandle) -> TurnStatus {
    let mut status = handle.status_watch();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(turn) = status.borrow_and_update().turn
            && turn.state.is_terminal()
        {
            return turn;
        }
        assert!(
            Instant::now() < deadline,
            "no terminal state inside the budget"
        );
        let _ = tokio::time::timeout(Duration::from_millis(100), status.changed()).await;
    }
}

// ───────── stream event shorthands ─────────

pub fn text(chunk: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        text: chunk.to_string(),
    }
}

pub fn done_end_turn() -> StreamEvent {
    StreamEvent::Done {
        stop: StopReason::EndTurn,
    }
}

pub fn tool_call(id: &str, name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::tool_call_start(id, name),
        StreamEvent::ToolCallDelta {
            id: id.into(),
            input_delta: "{}".into(),
        },
        StreamEvent::ToolCallEnd { id: id.into() },
        StreamEvent::Done {
            stop: StopReason::ToolUse,
        },
    ]
}

/// Text of the user messages of a request, in order. What the model actually
/// received, ephemeral context included.
pub fn user_texts(request: &CanonicalRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| matches!(m.role, agent_core::message::Role::User))
        .map(|m| m.text())
        .collect()
}

/// Events of an `AgentEvent` stream, in emission order, as short labels.
pub fn engine_labels(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            agent_runtime::thread::RuntimeEventPayload::Engine(event) => Some(match event {
                AgentEvent::Text(_) => "text".to_string(),
                AgentEvent::StreamReset => "stream_reset".to_string(),
                AgentEvent::ToolCall(_) => "tool_call".to_string(),
                AgentEvent::ToolResult(view) => format!("tool_result:{}", view.content),
                AgentEvent::EndTurn => "end_turn".to_string(),
                AgentEvent::Interrupted(..) => "interrupted".to_string(),
                AgentEvent::Error(_) => "error".to_string(),
                other => format!("{other:?}"),
            }),
            _ => None,
        })
        .collect()
}
