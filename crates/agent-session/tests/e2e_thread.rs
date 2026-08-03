//! End-to-end orchestration on the DURABLE store (US-019 AC2).
//!
//! The races of `agent-runtime` prove the ordering on an in-memory adapter; this
//! file proves the other half: that what the actor decided survives a real file.
//! Resume, fork, rewind, a child that fails and a parent that is cancelled each
//! run through the whole wiring (thread actor + `RunAgentRunner` + `run_agent` +
//! `JsonlThreadStore`), and the assertion is always the same one: reopening the
//! log rebuilds the same graph and the same transcript.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::message::{ContentBlock, Message};
use agent_core::provider::{
    CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderKind, StopReason, StreamEvent,
};
use agent_core::session::Session;
use agent_core::tools::{ModelToolResult, ToolDispatch, ToolEventSink, ToolInvocation};
use agent_core::{AgentContext, CancellationToken as CoreCancel, Deps, RunConfig};
use agent_runtime::agent::{AgentAuthority, AgentState};
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::id::{RandomIds, ThreadId, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::{RunAgentRunner, TurnRequest};
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::supervisor::{AgentSpawner, AgentSupervisor, ChildParts, ChildRequest};
use agent_runtime::thread::{MAX_PENDING_INPUTS, Submission, ThreadHandle, ThreadOptions};
use agent_session::JsonlThreadStore;
use agent_tokenizer::HeuristicCounter;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

// ───────── fixtures ─────────

struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pyxis-e2e-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

enum Scripted {
    Stream(Vec<StreamEvent>),
    /// Some events then a stream that never completes. Only a cancellation ends
    /// it, which is what a parent-cancelled scenario needs.
    Hang(Vec<StreamEvent>),
    OpenErr(ProviderError),
}

struct FakeProvider {
    caps: Capabilities,
    turns: Mutex<VecDeque<Scripted>>,
    fatal: bool,
}

impl FakeProvider {
    fn new(turns: Vec<Scripted>) -> Arc<Self> {
        Arc::new(Self {
            caps: Capabilities {
                tools: true,
                max_context: 100_000,
                ..Capabilities::default()
            },
            turns: Mutex::new(turns.into()),
            fatal: false,
        })
    }

    /// Every error is fatal instead of retryable: a test about a FAILED turn
    /// must not wait for three backoffs to observe it.
    fn failing(turns: Vec<Scripted>) -> Arc<Self> {
        let mut provider = Self::new(turns);
        Arc::get_mut(&mut provider).unwrap().fatal = true;
        provider
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
        _req: CanonicalRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>,
        ProviderError,
    > {
        match self.turns.lock().unwrap().pop_front() {
            Some(Scripted::Stream(events)) => Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            ))),
            Some(Scripted::Hang(events)) => Ok(Box::pin(
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
    fn classify_error(&self, _err: &ProviderError) -> ErrorClass {
        if self.fatal {
            ErrorClass::InvalidRequest
        } else {
            ErrorClass::Retryable
        }
    }
}

struct EchoTools;

#[async_trait::async_trait]
impl ToolDispatch for EchoTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        calls
            .into_iter()
            .map(|call| ModelToolResult::new(call.id, "ok".into(), false, true, None))
            .collect()
    }
}

struct InstantClock;

#[async_trait::async_trait]
impl Clock for InstantClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
    async fn sleep(&self, _dur: Duration) {}
}

fn turn_context(turn_id: TurnId) -> TurnContext {
    TurnContext {
        turn_id,
        model: "test-model".into(),
        reasoning_effort: None,
        model_runtime_fingerprint: None,
        permission_mode: "ask".into(),
        sandbox: "workspace-write".into(),
        workspace: PathBuf::from("/tmp/pyxis-e2e"),
        limits: TurnLimits {
            max_turns: 50,
            max_output_tokens: 4096,
            max_pending_inputs: MAX_PENDING_INPUTS,
        },
    }
}

fn text(chunk: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        text: chunk.to_string(),
    }
}

fn done() -> StreamEvent {
    StreamEvent::Done {
        stop: StopReason::EndTurn,
    }
}

/// Chains the turn on the transcript the store already holds, exactly like the
/// binary does: without it, a resume would be indistinguishable from a restart.
fn chained_context(
    conversation: Arc<Mutex<Vec<Message>>>,
) -> impl Fn(&TurnRequest) -> AgentContext + Send + Sync {
    move |request: &TurnRequest| {
        let mut messages = conversation
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default();
        messages.push(Message::user(request.text.clone()));
        let mut ctx = AgentContext::new(request.context.model.clone()).with_config(RunConfig {
            max_retries: 1,
            backoff_base_ms: 0,
            ..RunConfig::default()
        });
        ctx.messages = messages;
        ctx
    }
}

/// The snapshot layer of the binary, reduced to what an E2E needs: mirror what
/// the durable writer took, so the next turn chains on it.
struct MirroredSession {
    inner: Arc<dyn Session>,
    snapshot: Arc<Mutex<Vec<Message>>>,
}

#[async_trait::async_trait]
impl Session for MirroredSession {
    fn context_baseline(&self) -> Option<agent_core::ContextBaseline> {
        self.inner.context_baseline()
    }

    async fn sync(&self, messages: &[Message]) -> Result<(), agent_core::session::SessionError> {
        if let Ok(mut held) = self.snapshot.lock() {
            *held = messages.to_vec();
        }
        self.inner.sync(messages).await
    }
    async fn checkpoint(
        &self,
        kind: agent_core::compaction::CompactKind,
        messages: &[Message],
    ) -> Result<(), agent_core::session::SessionError> {
        if let Ok(mut held) = self.snapshot.lock() {
            *held = messages.to_vec();
        }
        self.inner.checkpoint(kind, messages).await
    }
    async fn record_context_transition(
        &self,
        transition: agent_core::ContextTransition,
    ) -> Result<(), agent_core::session::SessionError> {
        self.inner.record_context_transition(transition).await
    }
    async fn redact_encrypted_reasoning(&self) -> Result<(), agent_core::session::SessionError> {
        self.inner.redact_encrypted_reasoning().await
    }
    async fn record_file_snapshot(
        &self,
        snapshot: agent_core::session::FileSnapshot,
    ) -> Result<(), agent_core::session::SessionError> {
        self.inner.record_file_snapshot(snapshot).await
    }
}

struct Opened {
    handle: ThreadHandle,
    conversation: Arc<Mutex<Vec<Message>>>,
}

/// Opens (or resumes) a thread on a REAL JSONL log, wired exactly as the binary
/// wires it: the store is also the session writer, so there is one file, one
/// handle and one cursor.
async fn open(
    path: &Path,
    provider: Arc<FakeProvider>,
    agents: Option<Arc<AgentSupervisor>>,
    root: &CancellationToken,
) -> Opened {
    let store = Arc::new(JsonlThreadStore::open(path).expect("the log opens"));
    let thread_id = store.thread_id().expect("a thread id");
    let conversation = Arc::new(Mutex::new(Vec::new()));
    let session = Arc::new(MirroredSession {
        inner: Arc::clone(&store) as Arc<dyn Session>,
        snapshot: Arc::clone(&conversation),
    });
    let deps = Deps {
        provider,
        session,
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(InstantClock),
        tools: Arc::new(EchoTools),
        cancel: CoreCancel::new(),
        context_window: Default::default(),
    };
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store: Arc::clone(&store) as Arc<dyn ThreadStore>,
        runner: Arc::new(RunAgentRunner::new(
            deps,
            chained_context(Arc::clone(&conversation)),
        )),
        turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
            &RandomIds,
        )))) as Arc<dyn TurnContextSource>,
        ids: Arc::new(RandomIds),
        clock: Arc::new(InstantClock),
        parent_cancel: root.clone(),
        agents,
    })
    .await
    .expect("the thread starts");
    if let Ok(mut held) = conversation.lock() {
        *held = handle.resumed().messages.clone();
    }
    Opened {
        handle,
        conversation,
    }
}

async fn run_turn(opened: &Opened, prompt: &str) -> TurnState {
    opened
        .handle
        .submit(Submission::new(prompt))
        .await
        .expect("the input is accepted");
    let mut status = opened.handle.status_watch();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(turn) = status.borrow_and_update().turn
            && turn.state.is_terminal()
        {
            return turn.state;
        }
        assert!(tokio::time::Instant::now() < deadline, "no terminal state");
        let _ = tokio::time::timeout(Duration::from_millis(50), status.changed()).await;
    }
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(Message::text).collect()
}

// ───────── resume ─────────

/// A thread reopened rebuilds the same transcript and the same last state, and
/// the turn it chains next sees the whole history.
#[tokio::test]
async fn a_reopened_thread_rebuilds_its_transcript_and_its_last_state() {
    let workspace = Workspace::new("resume");
    let path = workspace.path("session.jsonl");
    let root = CancellationToken::new();

    let (thread_id, before) = {
        let opened = open(
            &path,
            FakeProvider::new(vec![Scripted::Stream(vec![text("bonjour"), done()])]),
            None,
            &root,
        )
        .await;
        assert_eq!(
            run_turn(&opened, "premier tour").await,
            TurnState::Completed
        );
        let messages = opened.conversation.lock().unwrap().clone();
        let thread_id = opened.handle.thread_id();
        opened.handle.shutdown().await;
        (thread_id, messages)
    };

    let reopened = open(
        &path,
        FakeProvider::new(vec![Scripted::Stream(vec![text("suite"), done()])]),
        None,
        &root,
    )
    .await;
    assert_eq!(
        reopened.handle.thread_id(),
        thread_id,
        "the identifier is bound to the log, not to the process"
    );
    assert_eq!(
        texts(&reopened.handle.resumed().messages),
        texts(&before),
        "the resumed transcript is the one that was persisted"
    );
    assert_eq!(
        reopened.handle.resumed().turn.map(|turn| turn.state),
        Some(TurnState::Completed)
    );

    assert_eq!(
        run_turn(&reopened, "second tour").await,
        TurnState::Completed
    );
    let after = reopened.conversation.lock().unwrap().clone();
    assert!(
        after.len() > before.len(),
        "the second turn chains on the first"
    );
    reopened.handle.shutdown().await;

    // The log itself is the proof: reading it back yields the same transcript.
    let store = JsonlThreadStore::open(&path).expect("the log reopens");
    assert_eq!(texts(&store.read().await.unwrap().messages), texts(&after));
}

/// A `TurnStarted` a crash left without a terminal is closed ONCE at resume, and
/// the transcript comes back with no tool call left unanswered.
///
/// The log is written by hand rather than by killing an actor: an actor that is
/// dropped still runs its shutdown and writes the terminal it owes, which is the
/// opposite of the case under test. What a crash leaves behind is exactly these
/// bytes, so these are the bytes the test starts from.
#[tokio::test]
async fn a_turn_left_open_by_a_crash_is_closed_once_at_resume() {
    let workspace = Workspace::new("recovery");
    let path = workspace.path("session.jsonl");
    let root = CancellationToken::new();

    let thread_id = ThreadId::generate(&RandomIds);
    let turn_id = TurnId::generate(&RandomIds);
    {
        let store = JsonlThreadStore::open(&path).expect("the log opens");
        store.create(&thread_id).await.expect("bind");
        for (seq, payload) in [
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id: None,
                text: "tour interrompu par un crash".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append(&agent_runtime::event::ThreadEvent {
                    event_id: agent_runtime::id::EventId::generate(&RandomIds),
                    thread_id,
                    seq: seq as u64 + 1,
                    at_ms: seq as u64,
                    payload,
                })
                .await
                .expect("append");
        }
        // A tool call the crash never answered: the next provider call would be
        // rejected outright if resume did not complete it.
        store
            .sync(&[
                Message::user("tour interrompu par un crash"),
                Message::assistant(vec![ContentBlock::tool_use(
                    "call_orphelin",
                    "bash",
                    serde_json::json!({}),
                )]),
            ])
            .await
            .expect("the transcript is written");
        store.close().await.expect("close");
    }

    let reopened = open(&path, FakeProvider::new(Vec::new()), None, &root).await;
    let resumed = reopened.handle.resumed();
    assert_eq!(
        resumed.recovered,
        vec![turn_id],
        "exactly one turn was closed by the recovery"
    );
    assert_eq!(
        resumed.reconciled_calls, 1,
        "the orphan tool call was answered before the recovery terminal"
    );
    assert_eq!(
        resumed.turn.map(|turn| turn.state),
        Some(TurnState::Interrupted)
    );
    reopened.handle.shutdown().await;

    // Reopening a second time recovers NOTHING: the repair was written once.
    let again = open(&path, FakeProvider::new(Vec::new()), None, &root).await;
    assert!(
        again.handle.resumed().recovered.is_empty(),
        "a recovery event is written once, not at every open"
    );
    assert_eq!(
        again.handle.resumed().turn.map(|turn| turn.state),
        Some(TurnState::Interrupted)
    );
    again.handle.shutdown().await;
}

// ───────── fork and rewind ─────────

/// A branch cut at a terminal boundary carries its provenance, holds the prefix
/// through the cut, and leaves the source byte-identical. Rewind is the same
/// operation aimed at an EARLIER turn: it branches, it never truncates.
#[tokio::test]
async fn a_branch_carries_its_provenance_and_never_touches_its_source() {
    let workspace = Workspace::new("branch");
    let path = workspace.path("session.jsonl");
    let root = CancellationToken::new();

    let opened = open(
        &path,
        FakeProvider::new(vec![
            Scripted::Stream(vec![text("un"), done()]),
            Scripted::Stream(vec![text("deux"), done()]),
        ]),
        None,
        &root,
    )
    .await;
    run_turn(&opened, "premier tour").await;
    let first_turn = opened.handle.status().turn.expect("a first turn").turn_id;
    run_turn(&opened, "second tour").await;
    let full = opened.conversation.lock().unwrap().clone();

    // Rewind: branch at the FIRST turn, while the thread already holds two.
    let rewound = opened.handle.fork(Some(first_turn)).await.expect("rewind");
    // Fork with no target: the LAST terminal boundary.
    let forked = opened.handle.fork(None).await.expect("fork");
    let source_bytes = std::fs::read(&path).expect("the source is readable");
    opened.handle.shutdown().await;
    assert_eq!(
        std::fs::read(&path).expect("the source survives"),
        source_bytes,
        "branching writes nothing into the source"
    );

    for (branch, expect_turns) in [(&rewound, 1usize), (&forked, 2usize)] {
        let snapshot = branch.store.read().await.expect("the branch reads");
        let origin = snapshot.origin.expect("a branch carries its origin");
        assert_eq!(origin.parent_thread_id, opened.handle.thread_id());
        assert_eq!(snapshot.thread_id, Some(branch.child_thread_id));
        let terminals = snapshot
            .events
            .iter()
            .filter(|event| {
                matches!(
                    &event.payload,
                    ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal()
                )
            })
            .count();
        assert_eq!(
            terminals, expect_turns,
            "the branch holds the prefix through its cut and nothing after it"
        );
    }
    assert_eq!(
        rewound.fork_turn_id, first_turn,
        "a rewind branches at the turn it was given"
    );

    // The branches are independent files. Deleting the source leaves them
    // readable, which is the whole point of materializing the prefix.
    let branch_path = agent_session::branch_path(&path, rewound.child_thread_id);
    drop(rewound);
    drop(forked);
    std::fs::remove_file(&path).expect("the source can be removed");

    let orphan = open(&branch_path, FakeProvider::new(Vec::new()), None, &root).await;
    assert_eq!(
        texts(&orphan.handle.resumed().messages),
        texts(&full[..full.len().min(orphan.handle.resumed().messages.len())]),
        "the branch replays the prefix it inherited"
    );
    assert!(
        !orphan.handle.resumed().messages.is_empty(),
        "an orphaned branch is still a readable conversation"
    );
    orphan.handle.shutdown().await;
}

// ───────── sub-agents ─────────

/// Builds children on their own JSONL logs, next to their parent's.
struct FileSpawner {
    dir: PathBuf,
    scripts: Mutex<VecDeque<Arc<FakeProvider>>>,
    logs: Mutex<Vec<(agent_runtime::id::AgentId, PathBuf)>>,
}

impl FileSpawner {
    fn new(dir: PathBuf, scripts: Vec<Arc<FakeProvider>>) -> Arc<Self> {
        Arc::new(Self {
            dir,
            scripts: Mutex::new(scripts.into()),
            logs: Mutex::new(Vec::new()),
        })
    }

    fn log_of(&self, agent_id: agent_runtime::id::AgentId) -> PathBuf {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == agent_id)
            .map(|(_, path)| path.clone())
            .expect("a log for this agent")
    }
}

#[async_trait::async_trait]
impl AgentSpawner for FileSpawner {
    async fn spawn(&self, request: &ChildRequest) -> Result<ChildParts, String> {
        let path = self.dir.join(format!("{}.jsonl", request.child_thread_id));
        let store = Arc::new(JsonlThreadStore::open(&path).map_err(|err| err.to_string())?);
        self.logs.lock().unwrap().push((request.agent_id, path));
        let provider = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| FakeProvider::new(Vec::new()));
        let conversation = Arc::new(Mutex::new(Vec::new()));
        let deps = Deps {
            provider,
            session: Arc::new(MirroredSession {
                inner: Arc::clone(&store) as Arc<dyn Session>,
                snapshot: Arc::clone(&conversation),
            }),
            tokenizer: Arc::new(HeuristicCounter),
            clock: Arc::new(InstantClock),
            tools: Arc::new(EchoTools),
            cancel: CoreCancel::new(),
            context_window: Default::default(),
        };
        Ok(ChildParts {
            store: store as Arc<dyn ThreadStore>,
            runner: Arc::new(RunAgentRunner::new(deps, chained_context(conversation))),
            turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
                &RandomIds,
            )))),
        })
    }
}

/// A child that fails leaves its parent and its siblings alive, and both logs
/// agree on what happened: the parent's graph names the failure, the child's own
/// log carries its terminal.
#[tokio::test]
async fn a_failing_child_leaves_the_parent_and_its_siblings_alive() {
    let workspace = Workspace::new("child-failed");
    let path = workspace.path("parent.jsonl");
    let root = CancellationToken::new();

    let spawner = FileSpawner::new(
        workspace.0.clone(),
        vec![
            FakeProvider::failing(vec![Scripted::OpenErr(ProviderError::Http {
                status: 400,
                message: "refus".into(),
                retry_after_ms: None,
            })]),
            FakeProvider::new(vec![Scripted::Stream(vec![text("ok"), done()])]),
        ],
    );
    let supervisor = AgentSupervisor::new(
        Arc::clone(&spawner) as Arc<dyn AgentSpawner>,
        Arc::new(RandomIds),
        Arc::new(InstantClock),
        AgentAuthority::read_only(),
    );
    let opened = open(
        &path,
        FakeProvider::new(Vec::new()),
        Some(Arc::clone(&supervisor)),
        &root,
    )
    .await;

    let doomed = supervisor
        .spawn("agent_1", "tâche qui échoue", &AgentAuthority::read_only())
        .await
        .expect("the spawn is accepted");
    let sibling = supervisor
        .spawn("agent_2", "tâche qui réussit", &AgentAuthority::read_only())
        .await
        .expect("the sibling spawns");

    // A child that finishes a turn goes `idle`, not terminal: it waits for a
    // follow-up. What frees its slot is leaving `running`, so that is what the
    // wait watches.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if supervisor.list().iter().all(|view| !view.state.is_active()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "children never ended"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let views = supervisor.list();
    let failed = views
        .iter()
        .find(|view| view.agent_id == doomed.agent_id)
        .expect("the failing child is listed");
    assert_eq!(failed.state, AgentState::Failed);
    let alive = views
        .iter()
        .find(|view| view.agent_id == sibling.agent_id)
        .expect("the sibling is listed");
    assert_eq!(
        alive.state,
        AgentState::Idle,
        "a failing child must not take its sibling down"
    );
    // The parent is still commandable: that is the whole invariant.
    assert_eq!(
        run_turn(&opened, "et le parent continue").await,
        TurnState::Completed
    );
    opened.handle.shutdown().await;

    // Both logs agree. The parent's graph is rebuilt from ITS log alone.
    let parent_log = JsonlThreadStore::open(&path).expect("the parent log reopens");
    let events = parent_log.read().await.unwrap().events;
    let failed_recorded = events.iter().any(|event| {
        matches!(
            &event.payload,
            ThreadEventPayload::AgentStateChanged { agent_id, to, .. }
                if *agent_id == doomed.agent_id && *to == AgentState::Failed
        )
    });
    assert!(
        failed_recorded,
        "the failure is durable in the parent's log"
    );

    let child_log =
        JsonlThreadStore::open(&spawner.log_of(doomed.agent_id)).expect("the child log reopens");
    let child_terminals: Vec<TurnState> = child_log
        .read()
        .await
        .unwrap()
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal() => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(
        child_terminals,
        vec![TurnState::Failed],
        "the child owns exactly one terminal, and it is its own"
    );
}

/// Cancelling the parent reaches every child before the parent writes its own
/// terminal, and no child is left active in either log.
#[tokio::test]
async fn cancelling_the_parent_closes_every_child_before_its_own_terminal() {
    let workspace = Workspace::new("parent-cancelled");
    let path = workspace.path("parent.jsonl");
    let root = CancellationToken::new();

    let spawner = FileSpawner::new(
        workspace.0.clone(),
        vec![
            FakeProvider::new(vec![Scripted::Hang(vec![text("...")])]),
            FakeProvider::new(vec![Scripted::Hang(vec![text("...")])]),
        ],
    );
    let supervisor = AgentSupervisor::new(
        Arc::clone(&spawner) as Arc<dyn AgentSpawner>,
        Arc::new(RandomIds),
        Arc::new(InstantClock),
        AgentAuthority::read_only(),
    );
    let opened = open(
        &path,
        FakeProvider::new(vec![Scripted::Hang(vec![text("...")])]),
        Some(Arc::clone(&supervisor)),
        &root,
    )
    .await;
    for index in 0..2 {
        supervisor
            .spawn(
                &format!("agent_{index}"),
                format!("tâche {index}"),
                &AgentAuthority::read_only(),
            )
            .await
            .expect("the spawn is accepted");
    }
    opened
        .handle
        .submit(Submission::new("le parent travaille aussi"))
        .await
        .expect("the input is accepted");

    let started = tokio::time::Instant::now();
    opened.handle.shutdown().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < agent_runtime::SHUTDOWN_DEADLINE,
        "the whole tree must close inside the shutdown budget, took {elapsed:?}"
    );
    assert!(
        supervisor
            .list()
            .iter()
            .all(|view| view.state.is_terminal()),
        "no child stays active once its parent is gone"
    );

    // The log is what a restart reads: no child left open, no phantom slot.
    let reopened = open(&path, FakeProvider::new(Vec::new()), None, &root).await;
    assert!(
        reopened
            .handle
            .resumed()
            .agents
            .iter()
            .all(|record| record.state.is_terminal()),
        "the rebuilt graph holds no active child"
    );
    assert_eq!(
        reopened.handle.resumed().turn.map(|turn| turn.state),
        Some(TurnState::Interrupted),
        "the parent's own turn ended interrupted, once"
    );
    reopened.handle.shutdown().await;
}

/// The memory adapter and the JSONL adapter answer the same questions. Kept here
/// because an E2E that only ever exercises one adapter cannot notice the day
/// they diverge.
#[tokio::test]
async fn the_two_adapters_agree_on_an_empty_thread() {
    let workspace = Workspace::new("adapters");
    let path = workspace.path("empty.jsonl");
    let jsonl = JsonlThreadStore::open(&path).expect("the log opens");
    let memory = MemoryThreadStore::new();
    let thread_id = ThreadId::generate(&RandomIds);

    jsonl.create(&thread_id).await.expect("create");
    memory.create(&thread_id).await.expect("create");

    let (from_file, from_memory) = (jsonl.read().await.unwrap(), memory.read().await.unwrap());
    assert_eq!(from_file.thread_id, from_memory.thread_id);
    assert_eq!(from_file.events, from_memory.events);
    assert_eq!(from_file.next_seq(), from_memory.next_seq());
}
