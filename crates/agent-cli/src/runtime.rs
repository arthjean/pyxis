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
use agent_core::model::{ModelToolMode, MultiAgentVersion};
use agent_core::provider::{Provider, ToolSpec};
use agent_core::session::Session;
use agent_core::step::StepContextSource;
use agent_core::tools::ToolDispatchSnapshot;
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

/// What the binary needs to give a thread its own sub-agent supervisor.
///
/// A supervisor belongs to ONE thread, so it cannot be built once and shared:
/// `/fork`, `/rewind` and a resume each open another thread and each get a
/// fresh one. What IS shared is the spawner (how a child is built) and the
/// handle the six tools address (see [`agent_tools::AgentHandle`]).
#[derive(Clone)]
pub struct AgentWiring {
    pub spawner: Arc<dyn agent_runtime::supervisor::AgentSpawner>,
    pub handle: Arc<agent_tools::AgentHandle>,
    /// Authority of the ROOT thread. A child never gets more (FR-11).
    pub authority: agent_runtime::AgentAuthority,
}

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
    /// Hosted web search requested by the user (`web_search` in the global
    /// settings). Off by default. Whether it is actually composed also depends
    /// on the provider and on the active model, which are read per step.
    pub web_search: bool,
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

    /// Tool mode of the model in force. `Direct` without a provider, which is
    /// what a test cell built with `new` should answer.
    pub fn tool_mode(&self) -> ModelToolMode {
        let Some(provider) = &self.provider else {
            return ModelToolMode::Direct;
        };
        let model = self.lock().model.clone();
        provider.tool_mode(&model)
    }

    /// Hosted web-search spec for the active model, or `None`.
    ///
    /// Three conditions, all required, and the order is the point: the user
    /// asked for it, the adapter can encode it, and the catalog declares the
    /// model able to use it. A missing provider answers `None`, like every other
    /// per-model question here.
    ///
    /// Only the TEXT variant is ever composed. Image results are a second
    /// capability (`WebSearchToolType::TextAndImage`) whose payloads reach the
    /// transcript as images the local budget did not plan for; asking for text
    /// keeps one contract instead of two.
    pub fn web_search_spec(&self) -> Option<agent_core::provider::ToolSpec> {
        let provider = self.provider.as_ref()?;
        if !self.lock().web_search {
            return None;
        }
        if !provider.capabilities().tool_calling.web_search {
            return None;
        }
        let model = self.lock().model.clone();
        let spec = agent_core::provider::ToolSpec::web_search(None, None, None);
        provider
            .model_tool_capabilities(&model)
            .ensure_tools_supported(std::slice::from_ref(&spec))
            .ok()?;
        Some(spec)
    }

    /// Multi-agent protocol of the model in force. `Disabled` without a
    /// provider: a model whose orchestration contract is unknown must not be
    /// handed orchestration tools (US-010 AC3).
    pub fn multi_agent_version(&self) -> MultiAgentVersion {
        let Some(provider) = &self.provider else {
            return MultiAgentVersion::Disabled;
        };
        let model = self.lock().model.clone();
        provider.multi_agent_version(&model)
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
        let overload_fallback_runtime = match (
            self.provider.as_ref(),
            settings.run_config.overload_fallback_model.as_deref(),
        ) {
            (Some(provider), Some(fallback)) if fallback != settings.model => Some(
                provider
                    .resolve_model_runtime(
                        fallback,
                        settings.reasoning_effort.as_deref(),
                        settings.run_config.max_output_tokens,
                        settings.run_config.max_retries,
                        settings.run_config.backoff_base_ms,
                    )
                    .map_err(|error| TurnContextError(error.to_string()))?,
            ),
            _ => None,
        };
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
                    max_output_tokens: settings.run_config.max_output_tokens,
                    max_pending_inputs: MAX_PENDING_INPUTS,
                },
            },
            model_runtime,
            overload_fallback_runtime,
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
    /// Code Mode wiring. `None` in a build or a run without a JavaScript
    /// runtime: the `exec`/`wait` pair is then never exposed and every direct
    /// model behaves exactly as before.
    code_mode: Option<Arc<agent_tools::CodeModeHandle>>,
    /// Read at each step to know which tool plan the active model wants. Held
    /// as the settings cell rather than a copied value, so a `/models` in
    /// session moves the plan at the next request without a second setter.
    settings: Option<Arc<SettingsCell>>,
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
            code_mode: None,
            settings: None,
        })
    }

    /// Attaches the Code Mode runtime and the settings the tool plan is read
    /// from. Without this, the source composes the direct plan it always has.
    pub fn with_code_mode(
        registry: Arc<agent_tools::Registry>,
        project: Vec<Message>,
        code_mode: Option<Arc<agent_tools::CodeModeHandle>>,
        settings: Arc<SettingsCell>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            state: Mutex::new(StepState {
                project,
                injections: Vec::new(),
            }),
            code_mode,
            settings: Some(settings),
        })
    }

    /// Follows the conversation: a thread that opens, forks or is resumed gets
    /// a NEW Code Mode session, and the previous one is shut down. A JavaScript
    /// session therefore never survives the thread it belonged to (US-009 AC3).
    pub async fn bind_thread(&self, thread: &ThreadId) {
        let Some(handle) = &self.code_mode else {
            return;
        };
        if let Err(detail) = handle.bind_thread(&thread.to_string()).await {
            tracing::warn!(%detail, "cannot open the code mode session of this thread");
        }
    }

    fn begin_turn(&self) {
        if let Some(handle) = &self.code_mode {
            handle.begin_turn();
        }
    }

    /// Closes the Code Mode session of the run, if any.
    pub async fn shutdown_code_mode(&self) {
        if let Some(handle) = &self.code_mode
            && let Some(report) = handle.shutdown().await
            && !report.joined
        {
            tracing::warn!(detail = ?report.detail, "code mode shutdown left a worker behind");
        }
    }

    fn tool_mode(&self) -> ModelToolMode {
        let Some(settings) = &self.settings else {
            return ModelToolMode::Direct;
        };
        settings.tool_mode()
    }

    fn multi_agent_version(&self) -> MultiAgentVersion {
        let Some(settings) = &self.settings else {
            return MultiAgentVersion::Disabled;
        };
        settings.multi_agent_version()
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

impl CliStepSource {
    /// Decides what the model SEES and what a nested call may dispatch, from
    /// the same captured snapshot.
    ///
    /// The two views share one dispatcher, so a tool hidden from a
    /// `code_mode_only` model still meets the same permissions, taint, hooks
    /// and cancellation it would meet as a direct call. `exec` and `wait` are
    /// removed from the direct plan of a model that does not orchestrate, so a
    /// direct model never sees a tool it cannot use.
    fn compose_tool_plan(
        &self,
        captured: ToolDispatchSnapshot,
    ) -> (Vec<ToolSpec>, Option<ToolDispatchSnapshot>) {
        let mode = self.tool_mode();
        // Codex keeps its legacy shell handler registered for internal callers,
        // but hides it whenever unified exec is model-visible. Exposing both
        // gives the model two overlapping contracts and makes it pick the
        // smaller legacy schema for ordinary commands.
        let captured = hide_legacy_shell_when_unified_exec(captured);
        // A model that does not drive the v2 protocol loses the six tools from
        // BOTH views, not just from what it sees: a nested call must not reach
        // an orchestration surface the model was never trained on, and a
        // historical direct model must keep exactly the plan it had (US-010 AC4).
        let captured = if self.multi_agent_version().drives_v2() {
            captured
        } else {
            let kept: Vec<ToolSpec> = captured
                .specs()
                .iter()
                .filter(|spec| !is_multi_agent_tool(spec))
                .cloned()
                .collect();
            captured.with_specs(kept)
        };
        let all = captured.specs().to_vec();
        let visible: Vec<ToolSpec> = match (&self.code_mode, mode.needs_code_mode()) {
            (Some(handle), true) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    handle.bind_step(
                        Arc::new(agent_code_mode::PlanDispatcher::new(
                            &captured,
                            &CODE_MODE_TOOLS,
                            handle.loop_guard(),
                            agent_core::tools::ToolEventSink::default(),
                            runtime,
                        )),
                        mode.hides_nested_tools(),
                    );
                }
                if mode.hides_nested_tools() {
                    all.iter()
                        .filter(|spec| is_code_mode_tool(spec))
                        .cloned()
                        .collect()
                } else {
                    all.clone()
                }
            }
            _ => all
                .iter()
                .filter(|spec| !is_code_mode_tool(spec))
                .cloned()
                .collect(),
        };
        // Hosted tools last: they are executed by the BACKEND, so they belong to
        // what the model sees and to nothing the registry dispatches. Adding it
        // to `visible` only, never to `dispatch`, is what keeps a nested Code
        // Mode call from trying to run a tool that has no local handler.
        let mut visible = visible;
        if let Some(spec) = self.settings.as_ref().and_then(|s| s.web_search_spec()) {
            visible.push(spec);
            visible.sort_by(|left, right| left.name.cmp(&right.name));
        }
        let dispatch = captured.with_specs(
            visible
                .iter()
                .filter(|spec| !is_hosted_tool(spec))
                .cloned()
                .collect(),
        );
        (visible, Some(dispatch))
    }
}

/// Executed by the provider, not by the registry. A hosted tool has no local
/// handler, so it must never enter a dispatch view.
fn is_hosted_tool(spec: &ToolSpec) -> bool {
    matches!(
        spec.kind,
        agent_core::provider::ToolKind::WebSearch { .. }
            | agent_core::provider::ToolKind::ToolSearch { .. }
    )
}

/// Model-facing Code Mode tools. A cell never calls them: one would recurse
/// into itself, the other would drive the session it runs in.
const CODE_MODE_TOOLS: [&str; 2] = [
    agent_code_mode::EXEC_TOOL_NAME,
    agent_code_mode::WAIT_TOOL_NAME,
];

fn is_code_mode_tool(spec: &ToolSpec) -> bool {
    CODE_MODE_TOOLS.contains(&spec.name.as_str())
}

fn hide_legacy_shell_when_unified_exec(captured: ToolDispatchSnapshot) -> ToolDispatchSnapshot {
    let unified_exec_visible = captured
        .specs()
        .iter()
        .any(|spec| spec.name == "exec_command");
    if !unified_exec_visible {
        return captured;
    }
    let visible = captured
        .specs()
        .iter()
        .filter(|spec| spec.name != "bash")
        .cloned()
        .collect();
    captured.with_specs(visible)
}

fn is_multi_agent_tool(spec: &ToolSpec) -> bool {
    agent_tools::MULTI_AGENT_V2_TOOLS.contains(&spec.name.as_str())
}

impl StepSource for CliStepSource {
    fn snapshot(&self) -> StepSnapshot {
        // The exposed tool set moves HERE, at a step boundary. A server that
        // connected while a turn was running becomes callable at the next model
        // request, and the request in flight keeps the catalog it was built with
        // (US-006 AC4).
        self.registry.commit_staged();
        let captured = self.registry.step_snapshot();
        let (tools, tool_dispatch) = self.compose_tool_plan(captured);
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
            sections.push(StepSection::skill(
                format!("injected:{name}"),
                Some(body.clone()),
            ));
        }
        StepSnapshot {
            tools,
            sections,
            tool_dispatch,
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
    /// Shared with the tool registry: the loop publishes its context budget
    /// here, `get_context_remaining` reads it. Default = a handle nobody else
    /// holds, which makes publishing a no-op.
    pub context_window: agent_core::budget::ContextWindowState,
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
    let (base, fallback_base, goal, mut config) = settings.read(|settings| {
        (
            crate::interactive::with_tool_guidelines(
                &request
                    .model_runtime
                    .as_ref()
                    .map(crate::prompt::select_system_prompt)
                    .unwrap_or_else(|| "You are a helpful assistant.".to_string()),
                &settings.tool_guidelines,
            ),
            request.overload_fallback_runtime.as_ref().map(|runtime| {
                crate::interactive::with_tool_guidelines(
                    &crate::prompt::select_system_prompt(runtime),
                    &settings.tool_guidelines,
                )
            }),
            settings.goal.clone(),
            settings.run_config.clone(),
        )
    });
    config.max_output_tokens = captured.limits.max_output_tokens;
    config.overload_fallback_runtime = request.overload_fallback_runtime.clone();
    let overload_fallback_system =
        fallback_base.map(|base| crate::interactive::compose_system(&base, goal.as_deref()));

    AgentContext {
        turn_id: None,
        model: captured.model.clone(),
        model_runtime: request.model_runtime.clone(),
        reasoning_effort: captured.reasoning_effort.clone(),
        system: Some(crate::interactive::compose_system(&base, goal.as_deref())),
        overload_fallback_system,
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
        agents: Option<&AgentWiring>,
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
            context_window: engine.context_window,
        };
        let step_source: Arc<dyn StepContextSource> = Arc::new(StepContexts::new(
            Arc::clone(&steps) as Arc<dyn StepSource>,
            Arc::clone(&ids),
        ));

        let runner = {
            let conversation = Arc::clone(&conversation);
            let settings = Arc::clone(&settings);
            let registry = Arc::clone(&registry);
            let turn_steps = Arc::clone(&steps);
            Arc::new(RunAgentRunner::new(engine_deps, move |request| {
                turn_steps.begin_turn();
                build_context(&conversation, &settings, &step_source, &registry, request)
            }))
        };

        // One supervisor per thread, bound to the tools right away: a call
        // arriving between two threads is refused instead of reaching the
        // previous conversation's children (US-011 AC3).
        let supervisor = agents.map(|wiring| {
            let supervisor = agent_runtime::supervisor::AgentSupervisor::new(
                Arc::clone(&wiring.spawner),
                Arc::clone(&ids),
                Arc::clone(&clock),
                wiring.authority.clone(),
            );
            wiring.handle.bind(Arc::clone(&supervisor));
            supervisor
        });
        let handle = ThreadHandle::start(ThreadOptions {
            thread_id,
            store,
            runner,
            turn_contexts: Arc::clone(&settings) as Arc<dyn TurnContextSource>,
            ids,
            clock,
            parent_cancel: parent_cancel.clone(),
            agents: supervisor,
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
            context_window: Default::default(),
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
            web_search: false,
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
            None,
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
                None,
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
            None,
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
            kind: Default::default(),
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

#[cfg(test)]
mod code_mode_plan_tests {
    //! US-009 AC1/AC2: what the model SEES per tool mode, and what a nested
    //! call may still dispatch.

    use std::sync::Arc;

    use agent_code_mode::nested::NestedToolBinding;
    use agent_code_mode::protocol::SessionId;
    use agent_code_mode::session::CodeModeSession;
    use agent_core::model::ModelToolMode;
    use agent_core::provider::{Capabilities, Provider, ProviderKind};
    use agent_tools::{CodeModeHandle, CodeModeSessionFactory, ExecTool, WaitTool};

    use super::*;

    /// Declares the tool mode and the orchestration protocol of every slug it
    /// is asked about.
    struct ModeProvider {
        caps: Capabilities,
        mode: ModelToolMode,
        multi_agent: agent_core::model::MultiAgentVersion,
    }

    #[async_trait::async_trait]
    impl Provider for ModeProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAiChatGpt
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn tool_mode(&self, _slug: &str) -> ModelToolMode {
            self.mode
        }
        fn multi_agent_version(&self, _slug: &str) -> agent_core::model::MultiAgentVersion {
            self.multi_agent
        }
        async fn stream(
            &self,
            _req: agent_core::provider::CanonicalRequest,
        ) -> Result<
            futures_util::stream::BoxStream<
                'static,
                Result<agent_core::provider::StreamEvent, agent_core::provider::ProviderError>,
            >,
            agent_core::provider::ProviderError,
        > {
            Err(agent_core::provider::ProviderError::Transport(
                "unused".into(),
            ))
        }
        async fn complete(
            &self,
            _req: agent_core::provider::CanonicalRequest,
        ) -> Result<agent_core::provider::CanonicalResponse, agent_core::provider::ProviderError>
        {
            Err(agent_core::provider::ProviderError::Transport(
                "unused".into(),
            ))
        }
        fn classify_error(
            &self,
            _err: &agent_core::provider::ProviderError,
        ) -> agent_core::provider::ErrorClass {
            agent_core::provider::ErrorClass::InvalidRequest
        }
    }

    struct NoEngineFactory;

    impl CodeModeSessionFactory for NoEngineFactory {
        fn open(&self, id: SessionId) -> Result<Arc<CodeModeSession>, String> {
            struct Inert;
            impl agent_code_mode::session::CellEngine for Inert {
                fn start(
                    &self,
                    _cell: agent_code_mode::protocol::CellId,
                    _request: &agent_code_mode::protocol::ExecuteRequest,
                    _sink: agent_code_mode::session::CellSink,
                ) -> Result<(), String> {
                    Ok(())
                }
                fn interrupt(&self, _cell: &agent_code_mode::protocol::CellId) {}
                fn shutdown(
                    &self,
                    _deadline: std::time::Duration,
                ) -> agent_code_mode::protocol::ShutdownReport {
                    agent_code_mode::protocol::ShutdownReport::joined()
                }
            }
            Ok(Arc::new(CodeModeSession::new(id, Arc::new(Inert))))
        }
    }

    fn source(mode: ModelToolMode) -> Arc<CliStepSource> {
        source_with(mode, agent_core::model::MultiAgentVersion::Disabled)
    }

    fn source_with(
        mode: ModelToolMode,
        multi_agent: agent_core::model::MultiAgentVersion,
    ) -> Arc<CliStepSource> {
        source_with_shell_surface(mode, multi_agent, false)
    }

    /// `codex_surface` registers the tools the UPSTREAM instructions name by
    /// hand (`exec_command`, `write_stdin`, `apply_patch`, and the legacy `bash`
    /// they replace). The remote prompt tells the model to reach for those
    /// spellings, so what happens to them under each tool mode is worth a test
    /// of its own.
    fn source_with_shell_surface(
        mode: ModelToolMode,
        multi_agent: agent_core::model::MultiAgentVersion,
        codex_surface: bool,
    ) -> Arc<CliStepSource> {
        let handle = Arc::new(CodeModeHandle::new(
            Arc::new(NoEngineFactory),
            NestedToolBinding::default(),
        ));
        let agents = Arc::new(agent_tools::AgentHandle::new());
        let mut registry =
            agent_tools::Registry::builder(std::env::temp_dir()).register(agent_tools::read::Read);
        if codex_surface {
            registry = registry
                .register(agent_tools::Bash)
                .register(agent_tools::ExecCommand)
                .register(agent_tools::WriteStdin)
                .register(agent_tools::ApplyPatch);
        }
        let registry = Arc::new(
            registry
                .register(ExecTool::new(Arc::clone(&handle)))
                .register(WaitTool::new(Arc::clone(&handle)))
                .register(agent_tools::SpawnAgent::new(Arc::clone(&agents)))
                .register(agent_tools::SendMessage::new(Arc::clone(&agents)))
                .register(agent_tools::FollowupTask::new(Arc::clone(&agents)))
                .register(agent_tools::ListAgents::new(Arc::clone(&agents)))
                .register(agent_tools::WaitAgent::new(Arc::clone(&agents)))
                .register(agent_tools::InterruptAgent::new(agents))
                .build(),
        );
        let settings = SettingsCell::with_provider(
            TurnSettings {
                model: "gpt-5.6-sol".into(),
                reasoning_effort: None,
                tool_guidelines: Vec::new(),
                goal: None,
                run_config: RunConfig::default(),
                permission_mode: "ask".into(),
                sandbox: "enforced (workspace)".into(),
                workspace: std::env::temp_dir(),
                web_search: false,
            },
            Arc::new(ModeProvider {
                caps: Capabilities::default(),
                mode,
                multi_agent,
            }),
        );
        CliStepSource::with_code_mode(registry, Vec::new(), Some(handle), settings)
    }

    fn names_of(specs: &[ToolSpec]) -> Vec<String> {
        let mut names: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
        names.sort();
        names
    }

    fn visible(mode: ModelToolMode) -> Vec<String> {
        names_of(&source(mode).snapshot().tools)
    }

    /// AC1: a `code_mode_only` model sees `exec` and `wait` and nothing else.
    #[tokio::test]
    async fn a_code_mode_only_model_sees_only_the_orchestration_pair() {
        assert_eq!(
            visible(ModelToolMode::CodeModeOnly),
            vec!["exec".to_string(), "wait".to_string()]
        );
    }

    #[tokio::test]
    async fn unified_exec_hides_the_legacy_shell_from_model_and_cells() {
        let source = source_with_shell_surface(
            ModelToolMode::CodeMode,
            agent_core::model::MultiAgentVersion::Disabled,
            true,
        );

        let snapshot = source.snapshot();
        let visible = names_of(&snapshot.tools);
        assert!(visible.contains(&"exec_command".to_string()), "{visible:?}");
        assert!(visible.contains(&"write_stdin".to_string()), "{visible:?}");
        assert!(!visible.contains(&"bash".to_string()), "{visible:?}");

        let nested = source
            .code_mode
            .as_ref()
            .map(|handle| {
                let mut names = handle
                    .catalog()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>();
                names.sort();
                names
            })
            .unwrap_or_default();

        assert!(nested.contains(&"exec_command".to_string()), "{nested:?}");
        assert!(nested.contains(&"write_stdin".to_string()), "{nested:?}");
        assert!(!nested.contains(&"bash".to_string()), "{nested:?}");
    }

    /// The upstream instructions tell the model to edit with `apply_patch` and
    /// to run commands with `exec_command`. On a `code_mode_only` model neither
    /// is directly callable, and `crate::prompt` answers that by pointing at the
    /// `tools` object. That answer is only true while both really sit in the
    /// nested catalog under those exact names: unregister one and the harness
    /// section starts promising a tool that is nowhere.
    #[tokio::test]
    async fn the_tool_names_the_instructions_use_stay_reachable_from_a_cell() {
        let source = source_with_shell_surface(
            ModelToolMode::CodeModeOnly,
            agent_core::model::MultiAgentVersion::Disabled,
            true,
        );
        assert_eq!(
            names_of(&source.snapshot().tools),
            vec!["exec".to_string(), "wait".to_string()],
            "the direct surface is the orchestration pair alone"
        );

        let nested = source
            .code_mode
            .as_ref()
            .map(|handle| {
                handle
                    .catalog()
                    .into_iter()
                    .map(|tool| tool.binding)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for named in ["apply_patch", "exec_command"] {
            assert!(
                nested.contains(&named.to_string()),
                "`{named}` is named by the instructions but absent from `tools`: {nested:?}"
            );
        }
    }

    /// AC2: a `code_mode` model keeps its direct tools AND the pair.
    #[tokio::test]
    async fn a_code_mode_model_keeps_its_direct_tools_and_the_pair() {
        assert_eq!(
            visible(ModelToolMode::CodeMode),
            vec!["exec".to_string(), "read".to_string(), "wait".to_string()]
        );
    }

    /// A direct model never sees a tool it cannot drive.
    #[tokio::test]
    async fn a_direct_model_never_sees_the_orchestration_pair() {
        assert_eq!(visible(ModelToolMode::Direct), vec!["read".to_string()]);
    }

    /// US-010 AC4 / US-011 AC1: the six v2 tools appear only for a model whose
    /// catalog entry says `v2`. A direct model on `disabled` keeps exactly the
    /// plan it had before this epic.
    #[tokio::test]
    async fn only_a_v2_model_is_handed_the_multi_agent_tools() {
        use agent_core::model::MultiAgentVersion;

        let disabled = source_with(ModelToolMode::Direct, MultiAgentVersion::Disabled).snapshot();
        assert_eq!(names_of(&disabled.tools), vec!["read".to_string()]);

        // v1 is a DIFFERENT baseline surface: it gets nothing rather than a
        // contract Pyxis invented.
        let v1 = source_with(ModelToolMode::Direct, MultiAgentVersion::V1).snapshot();
        assert_eq!(names_of(&v1.tools), vec!["read".to_string()]);

        let v2 = source_with(ModelToolMode::Direct, MultiAgentVersion::V2).snapshot();
        let mut expected = agent_tools::MULTI_AGENT_V2_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        expected.push("read".to_string());
        expected.sort();
        assert_eq!(names_of(&v2.tools), expected);
    }

    /// US-013 AC1: a `code_mode_only` v2 model sees only `exec` and `wait`, and
    /// the six tools stay DISPATCHABLE from a cell. Hiding them from the model
    /// must not remove them from the nested plan, or orchestration would exist
    /// in direct mode only.
    #[tokio::test]
    async fn a_code_mode_only_v2_model_keeps_the_tools_nested_dispatchable() {
        let source = source_with(
            ModelToolMode::CodeModeOnly,
            agent_core::model::MultiAgentVersion::V2,
        );
        let snapshot = source.snapshot();
        assert_eq!(
            names_of(&snapshot.tools),
            vec!["exec".to_string(), "wait".to_string()]
        );

        // What a cell may call: everything but the pair that would recurse.
        let runtime = tokio::runtime::Handle::current();
        let nested = agent_code_mode::PlanDispatcher::new(
            &source.registry.step_snapshot(),
            &super::CODE_MODE_TOOLS,
            agent_code_mode::NestedLoopGuard::default(),
            agent_core::tools::ToolEventSink::default(),
            runtime,
        );
        let callable = names_of(&agent_code_mode::nested::NestedToolDispatcher::catalog(
            &nested,
        ));
        for tool in agent_tools::MULTI_AGENT_V2_TOOLS {
            assert!(
                callable.contains(&tool.to_string()),
                "a cell must still reach `{tool}`: {callable:?}"
            );
        }
        assert!(!callable.contains(&"exec".to_string()));
    }

    /// The dispatch view and the visible view agree, so the model can never
    /// call something the plan would reject as unknown.
    #[tokio::test]
    async fn the_dispatch_view_matches_what_the_model_sees() {
        for mode in [
            ModelToolMode::Direct,
            ModelToolMode::CodeMode,
            ModelToolMode::CodeModeOnly,
        ] {
            let snapshot = source(mode).snapshot();
            let dispatch = snapshot.tool_dispatch.expect("a plan is always captured");
            let mut seen: Vec<String> = dispatch
                .specs()
                .iter()
                .map(|spec| spec.name.clone())
                .collect();
            let mut shown: Vec<String> = snapshot
                .tools
                .iter()
                .map(|spec| spec.name.clone())
                .collect();
            seen.sort();
            shown.sort();
            assert_eq!(seen, shown, "{mode:?}");
        }
    }
}

#[cfg(test)]
mod web_search_tests {
    use super::*;
    use agent_core::provider::ToolKind;

    /// The three gates on a hosted tool, and the reason each exists: the user
    /// asked (it reaches the network from the backend, where the local
    /// allow-list cannot see it), the adapter can encode it, and the catalog
    /// declared the model able to use it.
    #[test]
    fn a_cell_without_a_provider_never_composes_a_hosted_tool() {
        let settings = SettingsCell::new(TurnSettings {
            model: "m".into(),
            reasoning_effort: None,
            tool_guidelines: Vec::new(),
            goal: None,
            run_config: RunConfig::default(),
            permission_mode: "ask".into(),
            sandbox: "workspace-write".into(),
            workspace: std::env::temp_dir(),
            web_search: true,
        });
        assert!(
            settings.web_search_spec().is_none(),
            "a model whose contract is unknown must not be handed a hosted tool"
        );
    }

    /// A hosted tool is model-visible and never dispatchable: the registry has
    /// no handler for it, so letting one into a dispatch view would turn a
    /// backend-executed call into an "unknown tool" for any nested caller.
    #[test]
    fn hosted_tools_are_recognized_and_kept_out_of_dispatch() {
        assert!(is_hosted_tool(&ToolSpec::web_search(None, None, None)));
        assert!(is_hosted_tool(&ToolSpec::tool_search(
            "d",
            "client",
            serde_json::json!({"type": "object", "properties": {}})
        )));
        assert!(!is_hosted_tool(&ToolSpec::function(
            "read",
            "d",
            serde_json::json!({
                "type": "object", "properties": {}, "required": [],
                "additionalProperties": false
            })
        )));
        // A namespace of local tools is dispatched locally: it is not hosted.
        assert!(!is_hosted_tool(&ToolSpec {
            name: "srv".into(),
            description: "d".into(),
            kind: ToolKind::Namespace { tools: Vec::new() },
        }));
    }
}
