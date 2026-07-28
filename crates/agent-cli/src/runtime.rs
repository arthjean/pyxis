//! Wiring of the durable thread runtime into the binary (US-017, US-018).
//!
//! `agent-runtime` owns the thread; this module is the half of the contract that
//! only the binary can hold: which store a conversation writes to, how an
//! `AgentContext` is composed from the live session settings, and what the model
//! sees at each step. Everything else (mailbox, turn lifecycle, steering,
//! interruption, forks) is the runtime's and is not re-decided here.
//!
//! Two invariants shape the module:
//! - ONE durable file, ONE writer. The thread log IS the session log, so
//!   `run_agent` persists its transcript through the same handle that persists
//!   the orchestration events. Two writers on one session file would fight over
//!   its lock and interleave without a shared cursor.
//! - What a turn is committed to comes from its `TurnContext` capture, never
//!   from re-reading the settings mid-turn (US-006 AC1).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_core::clock::{Clock, SystemClock};
use agent_core::message::Message;
use agent_core::provider::Provider;
use agent_core::session::Session;
use agent_core::step::StepContextSource;
use agent_core::{AgentContext, Deps, RunConfig};
use agent_runtime::context::{
    CapturedTurnContext, StepContexts, StepSection, StepSnapshot, StepSource, TurnContext,
    TurnContextError, TurnContextSource, TurnLimits,
};
use agent_runtime::id::{IdGenerator, RandomIds, ThreadId, TurnId};
use agent_runtime::runner::{RunAgentRunner, TurnRequest};
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::thread::{
    Accepted, ForkError, MAX_PENDING_INPUTS, RuntimeEvent, Submission, SubmitError, ThreadHandle,
    ThreadOptions, ThreadStatus,
};
use agent_session::JsonlThreadStore;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::session::SharedSession;

/// Everything a turn is committed to, plus what the prompt is composed from.
///
/// Behind a mutex because `/models`, `/effort` and `/goal` move it BETWEEN turns
/// while the runtime reads it AT a turn start. The runtime is what makes that
/// safe: it captures once, and a turn already running keeps its capture.
#[derive(Debug, Clone)]
pub struct TurnSettings {
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Behavioral guidelines of the tools, folded into the system prompt.
    pub tool_guidelines: Vec<String>,
    /// Persistent session goal (`/goal`), re-composed into the system prompt.
    pub goal: Option<String>,
    pub run_config: RunConfig,
    /// Labels, not authority: the tool pipeline and the sandbox stay the
    /// deciders. Recorded so a transcript can be audited without replaying them.
    pub permission_mode: String,
    pub sandbox: String,
    pub workspace: PathBuf,
}

/// Shared, mutable settings cell. Also the runtime's [`TurnContextSource`].
pub struct SettingsCell {
    settings: Mutex<TurnSettings>,
    provider: Option<Arc<dyn Provider>>,
}

impl SettingsCell {
    #[cfg(test)]
    pub fn new(settings: TurnSettings) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings),
            provider: None,
        })
    }

    pub fn with_provider(settings: TurnSettings, provider: Arc<dyn Provider>) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings),
            provider: Some(provider),
        })
    }

    /// A poisoned lock is recovered rather than propagated: losing a setting
    /// degrades the next turn, it does not corrupt the log, and the runtime must
    /// not panic on a turn boundary.
    fn lock(&self) -> std::sync::MutexGuard<'_, TurnSettings> {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn read<T>(&self, f: impl FnOnce(&TurnSettings) -> T) -> T {
        f(&self.lock())
    }

    pub fn update(&self, f: impl FnOnce(&mut TurnSettings)) {
        f(&mut self.lock());
    }
}

impl TurnContextSource for SettingsCell {
    fn capture(&self, turn_id: TurnId) -> Result<CapturedTurnContext, TurnContextError> {
        let settings = self.lock();
        let model_runtime = self
            .provider
            .as_ref()
            .map(|provider| {
                provider.resolve_model_runtime(
                    &settings.model,
                    settings.reasoning_effort.as_deref(),
                    settings.run_config.max_output_tokens,
                    settings.run_config.max_retries,
                    settings.run_config.backoff_base_ms,
                )
            })
            .transpose()
            .map_err(|error| TurnContextError(error.to_string()))?;
        let effective_model = model_runtime
            .as_ref()
            .map(|runtime| runtime.slug.clone())
            .unwrap_or_else(|| settings.model.clone());
        let effective_reasoning = model_runtime
            .as_ref()
            .and_then(|runtime| runtime.reasoning_effort.clone())
            .or_else(|| settings.reasoning_effort.clone());
        Ok(CapturedTurnContext {
            context: TurnContext {
                turn_id,
                model: effective_model,
                reasoning_effort: effective_reasoning,
                model_runtime_fingerprint: model_runtime
                    .as_ref()
                    .map(|runtime| runtime.fingerprint.clone()),
                permission_mode: settings.permission_mode.clone(),
                sandbox: settings.sandbox.clone(),
                workspace: settings.workspace.clone(),
                limits: TurnLimits {
                    max_turns: settings.run_config.max_turns,
                    max_output_tokens: settings.run_config.max_output_tokens,
                    max_pending_inputs: MAX_PENDING_INPUTS,
                },
            },
            model_runtime,
        })
    }
}

/// Raw material of one model request, on the binary's side of the seam
/// (US-006 AC2): the tool catalog, the project context and the bodies injected
/// by `/<skill>` or `/init`.
///
/// The injections live HERE and not in `ephemeral_messages` for one reason: a
/// steer enters a turn that is already running, so a per-turn ephemeral slot
/// could never carry the body of a skill invoked mid-turn. As a step section it
/// reaches the next request whatever opened it, and it is still never persisted.
pub struct CliStepSource {
    registry: Arc<agent_tools::Registry>,
    state: Mutex<StepState>,
}

#[derive(Default)]
struct StepState {
    project: Vec<Message>,
    injections: Vec<(String, String)>,
}

impl CliStepSource {
    pub fn new(registry: Arc<agent_tools::Registry>, project: Vec<Message>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            state: Mutex::new(StepState {
                project,
                injections: Vec::new(),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StepState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces the project context (`/init` re-read, US-019 AC1 of the harness
    /// PRD). Visible from the NEXT model request on, not from the next turn.
    pub fn set_project(&self, project: Vec<Message>) {
        self.lock().project = project;
    }

    /// Adds a body for the rest of the current turn. A second invocation of the
    /// same name replaces it instead of injecting it twice.
    pub fn inject(&self, name: impl Into<String>, body: String) {
        let name = name.into();
        let mut state = self.lock();
        match state.injections.iter_mut().find(|(held, _)| *held == name) {
            Some(slot) => slot.1 = body,
            None => state.injections.push((name, body)),
        }
    }

    /// Drops one injected body by name. Used when the input that carried it was
    /// refused: the body must not leak into a turn the user never opened.
    pub fn remove_injection(&self, name: &str) {
        self.lock().injections.retain(|(held, _)| held != name);
    }

    /// Drops every injected body. Called when the thread goes IDLE, not at each
    /// terminal: a body invoked for a turn must not survive the conversation
    /// going quiet, but it must survive an input that opens the next turn right
    /// away.
    pub fn clear_injections(&self) {
        self.lock().injections.clear();
    }
}

impl StepSource for CliStepSource {
    fn snapshot(&self) -> StepSnapshot {
        // The exposed tool set moves HERE, at a step boundary. A server that
        // connected while a turn was running becomes callable at the next model
        // request, and the request in flight keeps the catalog it was built with
        // (US-006 AC4).
        self.registry.commit_staged();
        let tool_dispatch = self.registry.step_snapshot();
        let tools = tool_dispatch.specs().to_vec();
        let state = self.lock();
        let mut sections = Vec::with_capacity(state.project.len() + state.injections.len());
        // The environment block is the last project message and the only
        // volatile one: `StepContexts` puts every volatile section after the
        // stable ones, which is what keeps the cacheable prefix stable.
        let last = state.project.len().saturating_sub(1);
        for (index, message) in state.project.iter().enumerate() {
            let name = format!("project:{index}");
            let text = Some(message.text());
            sections.push(if index == last {
                StepSection::volatile(name, text)
            } else {
                StepSection::stable(name, text)
            });
        }
        for (name, body) in &state.injections {
            sections.push(StepSection::stable(
                format!("injected:{name}"),
                Some(body.clone()),
            ));
        }
        StepSnapshot {
            tools,
            sections,
            tool_dispatch: Some(tool_dispatch),
        }
    }
}

/// The I/O dependencies of the engine that do NOT depend on which conversation
/// is open. The session is deliberately absent: it IS the thread log, so the
/// runtime is what supplies it, and no caller can hand a writer pointed
/// somewhere else.
#[derive(Clone)]
pub struct EngineDeps {
    pub provider: Arc<dyn agent_core::provider::Provider>,
    pub tokenizer: Arc<dyn agent_tokenizer::TokenCounter>,
    pub clock: Arc<dyn Clock>,
    pub tools: Arc<dyn agent_core::tools::ToolDispatch>,
}

/// A branch the runtime materialized, named on the client's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub thread_id: ThreadId,
    pub fork_turn_id: TurnId,
    /// `None` for a thread that persists nothing.
    pub path: Option<PathBuf>,
}

/// One conversation: its durable log, its thread actor and the shared state the
/// turn engine is composed from.
pub struct SessionRuntime {
    handle: Arc<ThreadHandle>,
    path: Option<PathBuf>,
    conversation: Arc<Mutex<Vec<Message>>>,
    /// The transcript writer, as the engine sees it. Exposed because `/compact`
    /// and `/models` write a checkpoint and a redaction of their own, outside a
    /// turn: they go through the same writer rather than opening a second one.
    session: Arc<SharedSession>,
}

/// Composes the `AgentContext` of one turn.
///
/// Model, effort and limits come from the turn's own capture, never from the
/// live settings: that is what US-006 AC1 buys. The goal and the tool guidelines
/// are read live because they are not part of the captured configuration.
fn build_context(
    conversation: &Arc<Mutex<Vec<Message>>>,
    settings: &Arc<SettingsCell>,
    steps: &Arc<dyn StepContextSource>,
    registry: &Arc<agent_tools::Registry>,
    request: &TurnRequest,
) -> AgentContext {
    let mut messages = conversation
        .lock()
        .map(|held| held.clone())
        .unwrap_or_default();
    messages.push(Message::user(request.text.clone()));

    let captured = &request.context;
    let (base, goal, mut config) = settings.read(|settings| {
        (
            crate::interactive::with_tool_guidelines(
                request
                    .model_runtime
                    .as_ref()
                    .map(crate::prompt::select_system_prompt)
                    .unwrap_or("You are a helpful assistant."),
                &settings.tool_guidelines,
            ),
            settings.goal.clone(),
            settings.run_config.clone(),
        )
    });
    config.max_turns = captured.limits.max_turns;
    config.max_output_tokens = captured.limits.max_output_tokens;

    AgentContext {
        model: captured.model.clone(),
        model_runtime: request.model_runtime.clone(),
        reasoning_effort: captured.reasoning_effort.clone(),
        system: Some(crate::interactive::compose_system(&base, goal.as_deref())),
        messages,
        // Replaced by the first step frame; kept coherent so an estimate made
        // before that frame is not made on an empty catalog.
        tools: registry.tool_specs(),
        config,
        context_messages: Vec::new(),
        ephemeral_messages: Vec::new(),
        step_source: Some(Arc::clone(steps)),
        // Wired by `RunAgentRunner` from the turn's own queue: a client cannot
        // forget it and silently turn every steer into a post-turn message.
        inputs: None,
    }
}

impl SessionRuntime {
    /// Opens (or resumes) the thread whose durable log is `path`, or an
    /// in-memory one when `path` is `None` (`--ephemeral`, US-018 AC4).
    pub async fn open(
        path: Option<&Path>,
        engine: EngineDeps,
        registry: Arc<agent_tools::Registry>,
        settings: Arc<SettingsCell>,
        steps: Arc<CliStepSource>,
        parent_cancel: &CancellationToken,
    ) -> anyhow::Result<Self> {
        let ids: Arc<dyn IdGenerator> = Arc::new(RandomIds);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);

        let (store, session, conversation, thread_id) = match path {
            Some(path) => {
                let store = Arc::new(
                    JsonlThreadStore::open(path)
                        .map_err(|err| anyhow::anyhow!("session: {err}"))?,
                );
                let thread_id = store
                    .thread_id()
                    .map_err(|err| anyhow::anyhow!("session: {err}"))?;
                let (session, conversation) =
                    SharedSession::over(Arc::clone(&store) as Arc<dyn Session>);
                (
                    Arc::clone(&store) as Arc<dyn ThreadStore>,
                    session,
                    conversation,
                    thread_id,
                )
            }
            None => {
                let (session, conversation) = SharedSession::ephemeral();
                (
                    Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>,
                    session,
                    conversation,
                    ThreadId::generate(ids.as_ref()),
                )
            }
        };

        let engine_deps = Deps {
            provider: engine.provider,
            session: Arc::clone(&session) as Arc<dyn Session>,
            tokenizer: engine.tokenizer,
            clock: engine.clock,
            tools: engine.tools,
            // Replaced by the turn's own child token in `RunAgentRunner`: a
            // token never signalled leaves the loop behavior unchanged until
            // then, and no branch of the tree is orphaned.
            cancel: CancellationToken::new(),
        };
        let step_source: Arc<dyn StepContextSource> = Arc::new(StepContexts::new(
            Arc::clone(&steps) as Arc<dyn StepSource>,
            Arc::clone(&ids),
        ));

        let runner = {
            let conversation = Arc::clone(&conversation);
            let settings = Arc::clone(&settings);
            let registry = Arc::clone(&registry);
            Arc::new(RunAgentRunner::new(engine_deps, move |request| {
                build_context(&conversation, &settings, &step_source, &registry, request)
            }))
        };

        let handle = ThreadHandle::start(ThreadOptions {
            thread_id,
            store,
            runner,
            turn_contexts: Arc::clone(&settings) as Arc<dyn TurnContextSource>,
            ids,
            clock,
            parent_cancel: parent_cancel.clone(),
            // EP-004 built the supervisor and its five tools; exposing them in
            // the binary is not part of EP-005 and is tracked in CURRENT_STATUS.
            agents: None,
        })
        .await
        .map_err(|err| anyhow::anyhow!("thread: {err}"))?;

        // The transcript the runtime rebuilt (reconciled) is what the next turn
        // chains on. Seeded here rather than re-read by the client: the runtime
        // already answered "where did this conversation stop?".
        if let Ok(mut held) = conversation.lock() {
            *held = handle.resumed().messages.clone();
        }
        tracing::debug!(
            target: "pyxis::cli",
            thread_id = %thread_id,
            messages = handle.resumed().messages.len(),
            recovered = handle.resumed().recovered.len(),
            "thread runtime opened"
        );

        Ok(Self {
            handle: Arc::new(handle),
            path: path.map(Path::to_path_buf),
            conversation,
            session,
        })
    }

    pub fn thread_id(&self) -> ThreadId {
        self.handle.thread_id()
    }

    pub fn conversation(&self) -> &Arc<Mutex<Vec<Message>>> {
        &self.conversation
    }

    pub fn messages(&self) -> Vec<Message> {
        self.conversation
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    pub fn session(&self) -> &Arc<SharedSession> {
        &self.session
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.handle.subscribe()
    }

    pub fn status(&self) -> ThreadStatus {
        self.handle.status()
    }

    /// Submits an input that opens a turn of its own.
    pub async fn submit(&self, submission: Submission) -> Result<Accepted, SubmitError> {
        let started = Instant::now();
        let accepted = self.handle.submit(submission).await;
        self.trace_admission("submit", started, accepted.as_ref().ok());
        accepted
    }

    /// Steers the turn that is running (US-007). `expected` is the client's
    /// claim about what it is correcting.
    pub async fn steer(
        &self,
        submission: Submission,
        expected: Option<TurnId>,
    ) -> Result<Accepted, SubmitError> {
        let started = Instant::now();
        let accepted = self.handle.steer(submission, expected).await;
        self.trace_admission("steer", started, accepted.as_ref().ok());
        accepted
    }

    /// Signals the running turn. Returns as soon as the cancellation is sent,
    /// never after the terminal: the terminal is written after reconciliation.
    pub async fn interrupt(&self, turn_id: Option<TurnId>) -> Result<(), SubmitError> {
        let started = Instant::now();
        let signalled = self.handle.interrupt(turn_id).await?;
        tracing::debug!(
            target: "pyxis::cli",
            thread_id = %self.thread_id(),
            turn_id = signalled.map(|status| status.turn_id.to_string()),
            latency_ms = started.elapsed().as_millis() as u64,
            "interruption acknowledged"
        );
        Ok(())
    }

    /// Materializes a branch at a terminal turn boundary and names its file.
    ///
    /// The open handle the store hands back is dropped here: a client that
    /// switches to the branch reopens it, and holding two handles on one file
    /// would deadlock the switch on its own lock.
    pub async fn fork(&self, at: Option<TurnId>) -> Result<Branch, ForkError> {
        let started = Instant::now();
        let fork = self.handle.fork(at).await?;
        let branch = Branch {
            thread_id: fork.child_thread_id,
            fork_turn_id: fork.fork_turn_id,
            path: self
                .path
                .as_deref()
                .map(|parent| agent_session::branch_path(parent, fork.child_thread_id)),
        };
        drop(fork);
        tracing::debug!(
            target: "pyxis::cli",
            thread_id = %self.thread_id(),
            child_thread_id = %branch.thread_id,
            latency_ms = started.elapsed().as_millis() as u64,
            "branch materialized"
        );
        Ok(branch)
    }

    /// Closes admission, cancels, drains, then closes the store.
    pub async fn shutdown(&self) {
        let started = Instant::now();
        self.handle.shutdown().await;
        tracing::debug!(
            target: "pyxis::cli",
            thread_id = %self.thread_id(),
            cleanup_ms = started.elapsed().as_millis() as u64,
            "thread runtime closed"
        );
    }

    /// Admission trace (US-019 AC4). Identifiers and durations ONLY: a prompt
    /// never reaches a `debug` line.
    fn trace_admission(
        &self,
        operation: &'static str,
        started: Instant,
        accepted: Option<&Accepted>,
    ) {
        tracing::debug!(
            target: "pyxis::cli",
            thread_id = %self.thread_id(),
            operation,
            submission_id = accepted.map(|accepted| accepted.event_id.to_string()),
            turn_id = accepted.map(|accepted| accepted.turn_id.to_string()),
            latency_ms = started.elapsed().as_millis() as u64,
            "operation admitted"
        );
    }
}

/// Assembles the text of an assistant answer from the event stream.
///
/// Deltas are held back until they are COMMITTED, which is what a `StreamReset`
/// makes observable: an interrupted sampling drops what it had streamed, and a
/// client that wrote those bytes would report text the transcript does not carry
/// (US-018 AC6).
#[derive(Debug, Default)]
pub struct CommittedText {
    committed: String,
    pending: String,
}

impl CommittedText {
    pub fn observe(&mut self, event: &agent_core::AgentEvent) {
        match event {
            agent_core::AgentEvent::StreamReset => self.pending.clear(),
            agent_core::AgentEvent::Text(chunk) => self.pending.push_str(chunk),
            agent_core::AgentEvent::ToolCall(_) | agent_core::AgentEvent::EndTurn => {
                self.committed.push_str(&self.pending);
                self.pending.clear();
            }
            _ => {}
        }
    }

    pub fn into_text(self) -> String {
        self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::ToolCallView;
    use agent_core::provider::{
        CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
        ProviderKind, StopReason, StreamEvent,
    };
    use agent_runtime::lifecycle::TurnState;
    use agent_runtime::thread::Submission;

    /// Answers one scripted turn then ends. Enough to drive the real wiring:
    /// what is under test here is the runtime the binary assembles, not the
    /// engine, which has its own suite.
    struct OneTurnProvider {
        caps: Capabilities,
    }

    #[async_trait::async_trait]
    impl Provider for OneTurnProvider {
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
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    text: "réponse".into(),
                }),
                Ok(StreamEvent::Done {
                    stop: StopReason::EndTurn,
                }),
            ])))
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
        ) -> Result<CanonicalResponse, ProviderError> {
            Err(ProviderError::Transport("not used".into()))
        }
        fn classify_error(&self, _err: &ProviderError) -> ErrorClass {
            ErrorClass::InvalidRequest
        }
    }

    fn engine(registry: &Arc<agent_tools::Registry>) -> EngineDeps {
        EngineDeps {
            provider: Arc::new(OneTurnProvider {
                caps: Capabilities {
                    tools: true,
                    max_context: 100_000,
                    ..Capabilities::default()
                },
            }),
            tokenizer: Arc::new(agent_tokenizer::HeuristicCounter),
            clock: Arc::new(SystemClock),
            tools: Arc::clone(registry) as Arc<dyn agent_core::tools::ToolDispatch>,
        }
    }

    fn settings(workspace: &Path) -> Arc<SettingsCell> {
        SettingsCell::new(TurnSettings {
            model: "test-model".into(),
            reasoning_effort: None,
            tool_guidelines: Vec::new(),
            goal: None,
            run_config: RunConfig {
                max_retries: 1,
                backoff_base_ms: 0,
                ..RunConfig::default()
            },
            permission_mode: "ask".into(),
            sandbox: "enforced (workspace)".into(),
            workspace: workspace.to_path_buf(),
        })
    }

    /// US-018 AC1/AC4: an ephemeral run creates or resumes a thread through the
    /// same `ThreadHandle` as any other client, waits for its terminal state,
    /// and leaves NO file behind. Not a file that is cleaned up afterwards: one
    /// that is never opened.
    #[tokio::test]
    async fn an_ephemeral_thread_runs_a_full_turn_and_writes_no_file() {
        let dir = std::env::temp_dir().join(format!("pyxis-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let registry = Arc::new(agent_tools::Registry::builder(&dir).build());
        let steps = CliStepSource::new(Arc::clone(&registry), Vec::new());
        let root = CancellationToken::new();
        let runtime = SessionRuntime::open(
            None,
            engine(&registry),
            Arc::clone(&registry),
            settings(&dir),
            steps,
            &root,
        )
        .await
        .expect("an in-memory thread opens");

        let mut events = runtime.subscribe();
        let accepted = runtime
            .submit(Submission::new("bonjour"))
            .await
            .expect("the input is accepted");

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.expect("the live stream stays open");
                if let agent_runtime::thread::RuntimeEventPayload::TurnStateChanged { to, .. } =
                    event.payload
                    && to.is_terminal()
                    && event.turn_id == Some(accepted.turn_id)
                {
                    return to;
                }
            }
        })
        .await
        .expect("a terminal state inside the budget");
        assert_eq!(terminal, TurnState::Completed);

        // The transcript is rebuilt for the next turn without touching a disk.
        let messages = runtime.messages();
        assert!(
            messages.iter().any(|message| message.text() == "bonjour"),
            "the submitted input is in the transcript: {messages:?}"
        );
        runtime.shutdown().await;

        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "`--ephemeral` must open no file at all"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The durable variant of the same wiring: the thread log IS the session
    /// log, so one file carries both the transcript and the transitions, and
    /// reopening it resumes the conversation.
    #[tokio::test]
    async fn a_durable_thread_writes_its_transcript_into_its_own_log() {
        let dir = std::env::temp_dir().join(format!("pyxis-rt-durable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        let registry = Arc::new(agent_tools::Registry::builder(&dir).build());
        let root = CancellationToken::new();
        let thread_id = {
            let runtime = SessionRuntime::open(
                Some(&path),
                engine(&registry),
                Arc::clone(&registry),
                settings(&dir),
                CliStepSource::new(Arc::clone(&registry), Vec::new()),
                &root,
            )
            .await
            .expect("the thread opens");
            runtime
                .submit(Submission::new("premier"))
                .await
                .expect("accepted");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while !runtime
                .status()
                .turn
                .is_some_and(|turn| turn.state.is_terminal())
            {
                assert!(tokio::time::Instant::now() < deadline, "no terminal state");
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            let thread_id = runtime.thread_id();
            runtime.shutdown().await;
            thread_id
        };

        let resumed = SessionRuntime::open(
            Some(&path),
            engine(&registry),
            Arc::clone(&registry),
            settings(&dir),
            CliStepSource::new(Arc::clone(&registry), Vec::new()),
            &root,
        )
        .await
        .expect("the thread resumes");
        assert_eq!(
            resumed.thread_id(),
            thread_id,
            "the identity is bound to the log, not to the process"
        );
        assert!(
            resumed
                .messages()
                .iter()
                .any(|message| message.text() == "premier"),
            "the resumed transcript carries the first turn"
        );
        resumed.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tool_call() -> agent_core::AgentEvent {
        agent_core::AgentEvent::ToolCall(ToolCallView {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        })
    }

    /// US-018 AC6: what a reset dropped is never reported as an answer, and what
    /// was committed before it survives.
    #[test]
    fn reset_deltas_never_reach_the_committed_text() {
        let mut text = CommittedText::default();
        text.observe(&agent_core::AgentEvent::Text("premier ".into()));
        text.observe(&tool_call());
        text.observe(&agent_core::AgentEvent::Text("perdu".into()));
        text.observe(&agent_core::AgentEvent::StreamReset);
        text.observe(&agent_core::AgentEvent::Text("garde".into()));
        text.observe(&agent_core::AgentEvent::EndTurn);
        assert_eq!(text.into_text(), "premier garde");

        // Interrupted before any commit: nothing is claimed.
        let mut text = CommittedText::default();
        text.observe(&agent_core::AgentEvent::Text("partiel".into()));
        assert_eq!(text.into_text(), "");
    }

    /// The injected bodies belong to ONE turn, and the same name injected twice
    /// is one section, not two.
    #[test]
    fn injections_are_deduplicated_and_cleared_at_a_terminal() {
        let registry = Arc::new(agent_tools::Registry::builder(Path::new("/tmp")).build());
        let steps = CliStepSource::new(registry, vec![Message::user("<environment/>")]);

        steps.inject("skill:review", "corps v1".into());
        steps.inject("skill:review", "corps v2".into());
        let snapshot = steps.snapshot();
        let injected: Vec<&StepSection> = snapshot
            .sections
            .iter()
            .filter(|section| section.name.starts_with("injected:"))
            .collect();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].content.as_deref(), Some("corps v2"));

        steps.clear_injections();
        assert!(
            steps
                .snapshot()
                .sections
                .iter()
                .all(|section| !section.name.starts_with("injected:"))
        );
    }

    /// The single project message is the environment block: volatile, so the
    /// cacheable prefix does not move because the date did.
    #[test]
    fn the_last_project_section_is_the_volatile_one() {
        let registry = Arc::new(agent_tools::Registry::builder(Path::new("/tmp")).build());
        let steps = CliStepSource::new(
            registry,
            vec![Message::user("AGENTS"), Message::user("<environment/>")],
        );
        let snapshot = steps.snapshot();
        assert!(!snapshot.sections[0].volatile);
        assert!(snapshot.sections[1].volatile);
    }
}
