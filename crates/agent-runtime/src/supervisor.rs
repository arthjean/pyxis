//! Parent-owned sub-agents: spawn, observe, address, interrupt (US-013, US-014).
//!
//! The supervisor is the only thing that turns a reserved slot into a running
//! child. It owns nothing a thread already owns: each child IS a
//! [`crate::thread::ThreadHandle`], with its own durable log, its own mailbox
//! and its own turn lifecycle. What the supervisor adds is the parent side of
//! the relation: the graph accounting, the cancellation node the children hang
//! from, the handoff each terminal produces, and the refusals that keep one
//! parent from reaching another parent's child.
//!
//! Two seams keep the runtime free of provider and disk concerns:
//! - [`AgentSpawner`] builds the store, the engine and the context source of a
//!   child. The client implements it, because only the client knows how to
//!   build a tool registry restricted to an [`AgentAuthority`].
//! - [`AgentJournal`] records the filiation in the PARENT's log. A tool runs
//!   inside a turn task, not inside the actor, and the actor is the single
//!   writer of its log, so the record goes through the mailbox like any other
//!   operation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent::{
    AGENT_WAIT_WINDOW, AgentAuthority, AgentError, AgentGraph, AgentRecord, AgentState,
};
use crate::context::TurnContextSource;
use crate::event::ThreadEventPayload;
use crate::handoff::{AgentHandoff, HandoffDraft};
use crate::id::{AgentId, EventId, IdGenerator, ThreadId};
use crate::lifecycle::TurnState;
use crate::runner::TurnRunner;
use crate::store::ThreadStore;
use crate::thread::{
    RuntimeEventPayload, Submission, SubmitError, ThreadHandle, ThreadOptions, ThreadStatus,
    TurnStatus,
};

/// Characters a spawn task may carry. A task is a brief, not a transcript.
pub const MAX_AGENT_TASK: usize = 4_000;
/// How long the parent waits for its children to unwind at shutdown. One
/// budget for the whole tree, drained CONCURRENTLY with the parent's own tasks,
/// so sub-agents cost the thread no extra shutdown time (3 s NFR).
const CHILD_DRAIN: Duration = Duration::from_millis(1_000);

/// Durable sink of the parent's log.
///
/// Implemented by the thread actor: an agent event is persisted BEFORE the
/// spawn is acknowledged, like every other accepted operation (FR-05).
#[async_trait::async_trait]
pub trait AgentJournal: Send + Sync {
    async fn record(&self, payload: ThreadEventPayload) -> Result<EventId, SubmitError>;
}

/// Everything a client needs to build a child. Carries the GRANTED authority,
/// already intersected with the parent's: an implementation applies it, it never
/// recomputes it.
#[derive(Debug, Clone)]
pub struct ChildRequest {
    pub agent_id: AgentId,
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub task: String,
    pub authority: AgentAuthority,
}

/// The three pieces a child thread runs on. Same triple as a root thread: a
/// child is not a lesser object, only a bounded one.
pub struct ChildParts {
    pub store: Arc<dyn ThreadStore>,
    pub runner: Arc<dyn TurnRunner>,
    pub turn_contexts: Arc<dyn TurnContextSource>,
}

/// Builds the runtime pieces of a child (US-013 AC1).
///
/// An implementation MUST restrict the child's tools to `request.authority`.
/// The runtime computes and records that authority; enforcing it at the tool
/// boundary is the client's half of the contract.
#[async_trait::async_trait]
pub trait AgentSpawner: Send + Sync {
    async fn spawn(&self, request: &ChildRequest) -> Result<ChildParts, String>;
}

/// Identity handed back for an accepted spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpawned {
    pub agent_id: AgentId,
    pub thread_id: ThreadId,
    pub authority: AgentAuthority,
}

/// Identity handed back for an accepted parent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSent {
    pub agent_id: AgentId,
    /// True when the message steered a running turn instead of opening one.
    pub steered: bool,
}

/// What a listing shows. Deliberately NOT the transcript (US-013 AC3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub agent_id: AgentId,
    pub parent_thread_id: ThreadId,
    pub thread_id: ThreadId,
    pub task: String,
    pub state: AgentState,
    /// Human-readable authority, never a tool argument.
    pub authority: String,
    /// Turn the child is on, when it is running one.
    pub turn: Option<TurnStatus>,
    pub elapsed_ms: u64,
}

/// Answer of a bounded wait (US-013 AC4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// At least one child reached a handoff point. Each handoff is delivered
    /// exactly once.
    Ready(Vec<AgentHandoff>),
    /// The window elapsed with nothing terminal: the caller gets the state of
    /// the children rather than a block that never returns.
    Running(Vec<AgentView>),
}

/// Parent link, bound once the thread actor exists.
struct ParentLink {
    thread_id: ThreadId,
    journal: Arc<dyn AgentJournal>,
    /// Cancellation node the children hang from. A child of the PARENT thread's
    /// token: cancelling the parent reaches every child, and cancelling one
    /// child reaches neither its parent nor a sibling (FR-10).
    cancel: CancellationToken,
}

struct Child {
    handle: Arc<ThreadHandle>,
    cancel: CancellationToken,
}

/// Handoff waiting to be read, and whether it already was.
struct Pending {
    handoff: AgentHandoff,
    delivered: bool,
}

/// Owner of the children of one thread.
pub struct AgentSupervisor {
    graph: AgentGraph,
    spawner: Arc<dyn AgentSpawner>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    /// Authority this thread holds. A child never gets more (FR-16).
    authority: AgentAuthority,
    parent: OnceLock<ParentLink>,
    children: Mutex<HashMap<AgentId, Child>>,
    handoffs: Mutex<HashMap<AgentId, Pending>>,
    /// Parent messages already accepted, by idempotency key (US-014 AC5).
    accepted: Mutex<HashMap<String, AgentSent>>,
    /// Transitions the parent's log refused while it was closing. Handed back
    /// at shutdown so the actor, which is still the writer, can persist them
    /// itself instead of leaving the graph to be repaired at the next resume.
    unrecorded: Mutex<Vec<ThreadEventPayload>>,
    finished: Notify,
    tracker: TaskTracker,
}

impl AgentSupervisor {
    /// Supervisor of a root thread holding `authority`.
    pub fn new(
        spawner: Arc<dyn AgentSpawner>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        authority: AgentAuthority,
    ) -> Arc<Self> {
        Self::at_depth(spawner, ids, clock, authority, 0)
    }

    /// Supervisor of a thread `depth` levels below the root. At the depth limit
    /// every spawn is refused before anything is created.
    pub fn at_depth(
        spawner: Arc<dyn AgentSpawner>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        authority: AgentAuthority,
        depth: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            graph: AgentGraph::at_depth(depth),
            spawner,
            ids,
            clock,
            authority,
            parent: OnceLock::new(),
            children: Mutex::new(HashMap::new()),
            handoffs: Mutex::new(HashMap::new()),
            accepted: Mutex::new(HashMap::new()),
            unrecorded: Mutex::new(Vec::new()),
            finished: Notify::new(),
            tracker: TaskTracker::new(),
        })
    }

    /// Binds the supervisor to its thread. Called once by
    /// [`ThreadHandle::start`]; before it, every spawn is refused as detached.
    pub(crate) fn attach(
        &self,
        thread_id: ThreadId,
        journal: Arc<dyn AgentJournal>,
        cancel: CancellationToken,
    ) {
        if self
            .parent
            .set(ParentLink {
                thread_id,
                journal,
                cancel,
            })
            .is_err()
        {
            // A supervisor belongs to ONE thread: its graph, its slots and its
            // handoffs are that thread's. Rebinding would make a second thread
            // spend the first one's budget.
            tracing::warn!(
                thread_id = %thread_id,
                "sub-agent supervisor already bound to another thread; the binding is kept"
            );
        }
    }

    /// Authority this thread holds.
    pub fn authority(&self) -> &AgentAuthority {
        &self.authority
    }

    pub fn graph(&self) -> &AgentGraph {
        &self.graph
    }

    fn link(&self) -> Result<&ParentLink, AgentError> {
        self.parent.get().ok_or(AgentError::Detached)
    }

    fn lock_children(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Child>> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_handoffs(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Pending>> {
        self.handoffs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_accepted(&self) -> std::sync::MutexGuard<'_, HashMap<String, AgentSent>> {
        self.accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a transition through the parent's log, or keeps it for the
    /// actor when the mailbox is already closed.
    async fn journal(&self, payload: ThreadEventPayload) {
        let Ok(link) = self.link() else { return };
        if let Err(err) = link.journal.record(payload.clone()).await {
            tracing::debug!(error = %err, "sub-agent transition deferred to the thread actor");
            self.unrecorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload);
        }
    }

    /// Transitions the log could not take while the thread was closing. Drained
    /// by the actor during its own shutdown, when it is still the writer.
    pub(crate) fn take_unrecorded(&self) -> Vec<ThreadEventPayload> {
        std::mem::take(
            &mut *self
                .unrecorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Restores the graph of a resumed thread (US-012 AC5).
    pub(crate) fn restore(&self, records: Vec<AgentRecord>) {
        self.graph.restore(records);
        // A restored handoff is already history: marking it delivered is what
        // keeps a replayed terminal from being injected twice (US-015 AC6).
        let mut handoffs = self.lock_handoffs();
        for record in self.graph.records() {
            handoffs.insert(
                record.agent_id,
                Pending {
                    handoff: AgentHandoff::build(
                        record.agent_id,
                        record.thread_id,
                        record.state,
                        record.cause.clone(),
                        HandoffDraft::default(),
                    ),
                    delivered: true,
                },
            );
        }
    }

    /// Creates a child for `task` with the authority `request` asks for,
    /// narrowed to what this thread holds.
    ///
    /// Order is the contract: reserve atomically, persist the filiation, then
    /// create. A failure at any step frees the slot and leaves a durable cause
    /// behind (US-012 AC1/AC4).
    pub async fn spawn(
        self: &Arc<Self>,
        task: impl Into<String>,
        request: &AgentAuthority,
    ) -> Result<AgentSpawned, AgentError> {
        let link = self.link()?;
        let task = bounded_task(task.into());
        let authority = self.authority.grant(request);
        let agent_id = AgentId::generate(self.ids.as_ref());
        let child_thread_id = ThreadId::generate(self.ids.as_ref());

        self.graph.reserve(
            agent_id,
            child_thread_id,
            task.clone(),
            authority.clone(),
            self.clock.now_ms(),
        )?;

        // Durable BEFORE anything is created: a filiation the log does not carry
        // is a child nobody can account for after a restart.
        if let Err(err) = link
            .journal
            .record(ThreadEventPayload::AgentLinked {
                agent_id,
                child_thread_id,
                task: task.clone(),
                authority: authority.clone(),
            })
            .await
        {
            self.graph.transition(
                agent_id,
                AgentState::Failed,
                Some(format!("filiation not persisted: {err}")),
                self.clock.now_ms(),
            );
            return Err(err.into());
        }

        let request = ChildRequest {
            agent_id,
            parent_thread_id: link.thread_id,
            child_thread_id,
            task: task.clone(),
            authority: authority.clone(),
        };
        let parts = match self.spawner.spawn(&request).await {
            Ok(parts) => parts,
            Err(cause) => return Err(self.fail(agent_id, cause).await),
        };

        let cancel = link.cancel.child_token();
        let handle = match ThreadHandle::start(ThreadOptions {
            thread_id: child_thread_id,
            store: parts.store,
            runner: parts.runner,
            turn_contexts: parts.turn_contexts,
            ids: Arc::clone(&self.ids),
            clock: Arc::clone(&self.clock),
            parent_cancel: cancel.clone(),
            agents: None,
        })
        .await
        {
            Ok(handle) => Arc::new(handle),
            Err(err) => return Err(self.fail(agent_id, err.to_string()).await),
        };

        // Subscribed BEFORE the first turn opens: a broadcast delivers nothing
        // that predates its receiver, and a child that answers instantly would
        // otherwise hand back an empty summary.
        let events = handle.subscribe();
        if let Err(err) = handle.submit(Submission::new(task)).await {
            handle.shutdown().await;
            return Err(self.fail(agent_id, err.to_string()).await);
        }

        self.lock_children().insert(
            agent_id,
            Child {
                handle: Arc::clone(&handle),
                cancel: cancel.clone(),
            },
        );
        let supervisor = Arc::downgrade(self);
        self.tracker
            .spawn(async move { watch(supervisor, agent_id, handle, cancel, events).await });

        tracing::debug!(
            target: "pyxis::runtime",
            thread_id = %link.thread_id,
            agent_id = %agent_id,
            child_thread_id = %child_thread_id,
            authority = %authority.label(),
            "sub-agent spawned"
        );
        Ok(AgentSpawned {
            agent_id,
            thread_id: child_thread_id,
            authority,
        })
    }

    /// Frees the slot of a spawn that could not be created and records why
    /// (US-012 AC4).
    async fn fail(&self, agent_id: AgentId, cause: String) -> AgentError {
        self.graph.transition(
            agent_id,
            AgentState::Failed,
            Some(cause.clone()),
            self.clock.now_ms(),
        );
        self.journal(ThreadEventPayload::AgentStateChanged {
            agent_id,
            to: AgentState::Failed,
            cause: Some(cause.clone()),
        })
        .await;
        self.finished.notify_waiters();
        AgentError::Spawn(cause)
    }

    /// Every child of this thread, without a byte of their transcripts
    /// (US-013 AC3).
    pub fn list(&self) -> Vec<AgentView> {
        let parent_thread_id = self.parent.get().map(|link| link.thread_id);
        let children = self.lock_children();
        let now = self.clock.now_ms();
        self.graph
            .records()
            .into_iter()
            .map(|record| AgentView {
                agent_id: record.agent_id,
                parent_thread_id: parent_thread_id.unwrap_or(record.thread_id),
                thread_id: record.thread_id,
                task: record.task,
                state: record.state,
                authority: record.authority.label(),
                turn: children
                    .get(&record.agent_id)
                    .and_then(|child| child.handle.status().turn),
                elapsed_ms: record
                    .ended_at_ms
                    .unwrap_or(now)
                    .saturating_sub(record.started_at_ms),
            })
            .collect()
    }

    /// Waits up to [`AGENT_WAIT_WINDOW`] for a handoff.
    pub async fn wait(&self, agent_id: Option<AgentId>) -> Result<WaitOutcome, AgentError> {
        self.wait_within(agent_id, AGENT_WAIT_WINDOW).await
    }

    /// Same, with an explicit window. Refuses an agent this thread does not own
    /// before waiting at all.
    pub async fn wait_within(
        &self,
        agent_id: Option<AgentId>,
        window: Duration,
    ) -> Result<WaitOutcome, AgentError> {
        if let Some(agent_id) = agent_id
            && self.graph.get(agent_id).is_none()
        {
            return Err(AgentError::Unreachable { agent_id });
        }
        let deadline = Instant::now() + window;
        loop {
            // Registered BEFORE the state is read: a terminal landing between
            // the two would otherwise notify nobody and cost a whole window.
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let ready = self.take_ready(agent_id);
            if !ready.is_empty() {
                return Ok(WaitOutcome::Ready(ready));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Ok(WaitOutcome::Running(self.list()));
            }
        }
    }

    /// Takes the handoffs not delivered yet. Each one crosses to the parent
    /// exactly once (US-015 AC6).
    fn take_ready(&self, agent_id: Option<AgentId>) -> Vec<AgentHandoff> {
        let mut handoffs = self.lock_handoffs();
        let mut ready = Vec::new();
        // Creation order, so a listing and a wait tell the same story.
        for record in self.graph.records() {
            if agent_id.is_some_and(|wanted| wanted != record.agent_id) {
                continue;
            }
            if let Some(pending) = handoffs.get_mut(&record.agent_id)
                && !pending.delivered
            {
                pending.delivered = true;
                ready.push(pending.handoff.clone());
            }
        }
        ready
    }

    /// Sends a message to a child (US-014).
    ///
    /// A running child is STEERED, through the same protocol a user steers with;
    /// an idle one opens a new turn. Which of the two happened is reported, not
    /// decided by the caller: only the supervisor knows the child's state at the
    /// instant the message is accepted.
    pub async fn send(
        &self,
        agent_id: AgentId,
        text: impl Into<String>,
        client_message_id: Option<String>,
    ) -> Result<AgentSent, AgentError> {
        if let Some(key) = &client_message_id
            && let Some(prior) = self.lock_accepted().get(key).copied()
        {
            // A replayed follow-up returns what the first one was given and
            // reaches no child (US-014 AC5).
            return Ok(prior);
        }
        let record = self.reachable(agent_id)?;
        let handle = self.handle_of(agent_id)?;
        let submission = Submission {
            text: text.into(),
            client_message_id: client_message_id.clone(),
        };

        let sent = if record.state.is_active() {
            handle.steer(submission, None).await?;
            AgentSent {
                agent_id,
                steered: true,
            }
        } else {
            // A slot first: a child coming back to life costs the same
            // concurrency as a new one.
            self.graph.resume(agent_id)?;
            match handle.submit(submission).await {
                Ok(_) => AgentSent {
                    agent_id,
                    steered: false,
                },
                Err(err) => {
                    self.graph
                        .transition(agent_id, AgentState::Idle, None, self.clock.now_ms());
                    return Err(err.into());
                }
            }
        };
        if let Some(key) = client_message_id {
            self.lock_accepted().insert(key, sent);
        }
        Ok(sent)
    }

    /// Interrupts one child (US-014 AC3).
    ///
    /// Signals its running turn, then closes its cancellation node. Only that
    /// branch: a sibling and the parent keep running, which is the whole point
    /// of hanging every child off a token of its own.
    pub async fn interrupt(&self, agent_id: AgentId) -> Result<(), AgentError> {
        self.reachable(agent_id)?;
        let handle = self.handle_of(agent_id)?;
        let cancel = self
            .lock_children()
            .get(&agent_id)
            .map(|child| child.cancel.clone());
        let _ = handle.interrupt(None).await;
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        tracing::debug!(target: "pyxis::runtime", agent_id = %agent_id, "sub-agent interruption signalled");
        Ok(())
    }

    /// Cancels every child and waits for them to unwind (US-013 AC5).
    ///
    /// Called by the thread actor BEFORE it writes its own terminal, so no
    /// child outlives the parent that owns it. Bounded: a child that ignores
    /// its signal must not spend the parent's whole shutdown budget.
    pub(crate) async fn shutdown(&self) {
        if let Some(link) = self.parent.get() {
            link.cancel.cancel();
        }
        self.tracker.close();
        let children: Vec<Arc<ThreadHandle>> = self
            .lock_children()
            .drain()
            .map(|(_, child)| child.handle)
            .collect();
        let drained = tokio::time::timeout(CHILD_DRAIN, async {
            self.tracker.wait().await;
            // Each child's actor already saw the cancellation above, so these
            // awaits collect finished tasks rather than wait on live ones.
            for handle in children {
                handle.shutdown().await;
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!("sub-agents did not unwind inside {CHILD_DRAIN:?}");
        }
    }

    /// The record of a child this thread owns and can still address.
    ///
    /// Unknown, terminal and foreign all collapse into one refusal: telling
    /// them apart would let a parent probe another parent's graph
    /// (US-014 AC4, edge case #16).
    fn reachable(&self, agent_id: AgentId) -> Result<AgentRecord, AgentError> {
        match self.graph.get(agent_id) {
            Some(record) if record.state.accepts_input() => Ok(record),
            _ => Err(AgentError::Unreachable { agent_id }),
        }
    }

    fn handle_of(&self, agent_id: AgentId) -> Result<Arc<ThreadHandle>, AgentError> {
        self.lock_children()
            .get(&agent_id)
            .map(|child| Arc::clone(&child.handle))
            .ok_or(AgentError::Unreachable { agent_id })
    }

    /// Closes a child WITHOUT touching its handoff.
    ///
    /// Used when a child that already reported is torn down: being closed
    /// afterwards must not erase what it produced and its parent has not read
    /// yet.
    async fn close(&self, agent_id: AgentId, cause: Option<String>) {
        if self.graph.transition(
            agent_id,
            AgentState::Interrupted,
            cause.clone(),
            self.clock.now_ms(),
        ) {
            self.journal(ThreadEventPayload::AgentStateChanged {
                agent_id,
                to: AgentState::Interrupted,
                cause,
            })
            .await;
            self.lock_children().remove(&agent_id);
            self.finished.notify_waiters();
        }
    }

    /// Publishes what a child owes its parent at a handoff point.
    ///
    /// Transition first, handoff second: the graph refuses a second terminal,
    /// so a race between the watcher and a shutdown cannot publish twice.
    async fn publish(
        &self,
        agent_id: AgentId,
        state: AgentState,
        cause: Option<String>,
        draft: HandoffDraft,
    ) {
        let Some(record) = self.graph.get(agent_id) else {
            return;
        };
        if !self
            .graph
            .transition(agent_id, state, cause.clone(), self.clock.now_ms())
        {
            return;
        }
        // The result is owed to the caller whatever the log does: a mailbox
        // closed by a shutdown defers the write, it does not drop the handoff.
        self.journal(ThreadEventPayload::AgentStateChanged {
            agent_id,
            to: state,
            cause: cause.clone(),
        })
        .await;
        let handoff = AgentHandoff::build(agent_id, record.thread_id, state, cause, draft);
        self.lock_handoffs().insert(
            agent_id,
            Pending {
                handoff,
                delivered: false,
            },
        );
        if state.is_terminal() {
            self.lock_children().remove(&agent_id);
        }
        self.finished.notify_waiters();
    }
}

/// Bounds the brief a child is spawned with.
fn bounded_task(task: String) -> String {
    match task.char_indices().nth(MAX_AGENT_TASK) {
        None => task,
        Some((cut, _)) => task[..cut].to_string(),
    }
}

/// Collects what a child's handoff will be made of, from its live events.
///
/// Only COMMITTED text lands in the summary: deltas of a stream the engine
/// reset were never part of an assistant message, and a summary is not the
/// place to resurrect them.
#[derive(Default)]
struct DraftCollector {
    committed: String,
    pending: String,
    artifacts: Vec<String>,
    diff: Option<String>,
}

impl DraftCollector {
    fn observe(&mut self, payload: &RuntimeEventPayload) {
        let RuntimeEventPayload::Engine(event) = payload else {
            return;
        };
        match event {
            AgentEvent::Text(chunk) => self.pending.push_str(chunk),
            AgentEvent::StreamReset => self.pending.clear(),
            AgentEvent::ModelTurn(_) | AgentEvent::EndTurn => {
                if !self.pending.is_empty() {
                    self.committed = std::mem::take(&mut self.pending);
                }
            }
            AgentEvent::TurnDiff(view) => {
                self.artifacts = view.files.iter().map(|f| f.path.clone()).collect();
                self.diff = Some(
                    view.files
                        .iter()
                        .filter_map(|f| f.unified.clone())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            _ => {}
        }
    }

    /// Takes the draft of the turn that just ended and re-arms for the next one.
    fn take(&mut self) -> HandoffDraft {
        self.pending.clear();
        HandoffDraft {
            summary: std::mem::take(&mut self.committed),
            artifacts: std::mem::take(&mut self.artifacts),
            diff: self.diff.take(),
        }
    }
}

/// Agent state a turn terminal maps to.
///
/// A completed turn leaves the child ADDRESSABLE: it published its result and
/// can take a follow-up. The two other terminals close it for good.
fn agent_state_of(turn: TurnState) -> AgentState {
    match turn {
        TurnState::Completed => AgentState::Idle,
        TurnState::Failed => AgentState::Failed,
        _ => AgentState::Interrupted,
    }
}

/// Follows one child until it is closed.
///
/// Publishes a handoff at EVERY terminal turn, not only the last one, so a
/// child that took a follow-up hands back a result per turn. Ends on a terminal
/// agent state, on a cancellation, or when the child's channels close, and in
/// the last two cases it still publishes: a parent must never be left waiting
/// on a child that quietly disappeared.
async fn watch(
    supervisor: Weak<AgentSupervisor>,
    agent_id: AgentId,
    handle: Arc<ThreadHandle>,
    cancel: CancellationToken,
    mut events: broadcast::Receiver<crate::thread::RuntimeEvent>,
) {
    let mut status = handle.status_watch();
    let mut draft = DraftCollector::default();
    let mut published: Option<crate::id::TurnId> = None;

    loop {
        // Fold in everything already buffered BEFORE reading the state: the
        // engine broadcasts its text before the actor publishes the terminal,
        // so a handoff built without draining first would be empty for a child
        // that answered in one go.
        loop {
            match events.try_recv() {
                Ok(event) => draft.observe(&event.payload),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        let snapshot: ThreadStatus = status.borrow_and_update().clone();
        if let Some(turn) = snapshot.turn
            && turn.state.is_terminal()
            && snapshot.pending_inputs == 0
            && snapshot.pending_steers == 0
            && published != Some(turn.turn_id)
        {
            published = Some(turn.turn_id);
            let state = agent_state_of(turn.state);
            let Some(supervisor) = supervisor.upgrade() else {
                return;
            };
            supervisor
                .publish(agent_id, state, cause_of(turn.state), draft.take())
                .await;
            if state.is_terminal() {
                handle.shutdown().await;
                return;
            }
        }

        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(event) => {
                    draft.observe(&event.payload);
                    continue;
                }
                // A lagging watcher lost live deltas, not the durable state: the
                // summary may be shorter, the handoff still happens.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            () = cancel.cancelled() => break,
        }
    }

    // The child is gone without a terminal turn of its own. A child that never
    // reported owes its parent an interrupted handoff rather than silence; one
    // that already reported keeps that report, because being torn down does not
    // erase a result its parent has not read yet.
    if let Some(supervisor) = supervisor.upgrade() {
        let cause = "interrupted: sub-agent stopped".to_string();
        let reported = supervisor
            .graph()
            .get(agent_id)
            .is_some_and(|record| !record.state.is_active());
        if reported {
            supervisor.close(agent_id, Some(cause)).await;
        } else {
            supervisor
                .publish(agent_id, AgentState::Interrupted, Some(cause), draft.take())
                .await;
        }
    }
    handle.shutdown().await;
}

fn cause_of(turn: TurnState) -> Option<String> {
    match turn {
        TurnState::Completed => None,
        TurnState::Failed => Some("the sub-agent turn failed".into()),
        other => Some(format!("the sub-agent turn ended `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{FileChange, FileDiffView, ModelTurnView, TurnDiffView};

    fn engine(event: AgentEvent) -> RuntimeEventPayload {
        RuntimeEventPayload::Engine(event)
    }

    /// Only committed text reaches a summary: a reset stream leaves nothing.
    #[test]
    fn a_reset_stream_contributes_nothing_to_the_summary() {
        let mut draft = DraftCollector::default();
        draft.observe(&engine(AgentEvent::Text("faux départ".into())));
        draft.observe(&engine(AgentEvent::StreamReset));
        draft.observe(&engine(AgentEvent::Text("la vraie".into())));
        draft.observe(&engine(AgentEvent::Text(" réponse".into())));
        draft.observe(&engine(AgentEvent::EndTurn));

        let taken = draft.take();
        assert_eq!(taken.summary, "la vraie réponse");
    }

    /// The LAST committed message wins, and a taken draft re-arms empty.
    #[test]
    fn the_last_committed_message_is_the_summary_and_the_draft_rearms() {
        let mut draft = DraftCollector::default();
        draft.observe(&engine(AgentEvent::Text("premier".into())));
        draft.observe(&engine(AgentEvent::ModelTurn(ModelTurnView::default())));
        draft.observe(&engine(AgentEvent::Text("second".into())));
        draft.observe(&engine(AgentEvent::EndTurn));
        assert_eq!(draft.take().summary, "second");
        assert_eq!(draft.take().summary, "", "a taken draft starts over");
    }

    /// AC1: a diff turns into artifacts plus a hash, never into inlined content.
    #[test]
    fn a_turn_diff_becomes_artifacts_and_a_hash() {
        let mut draft = DraftCollector::default();
        draft.observe(&engine(AgentEvent::TurnDiff(TurnDiffView {
            files: vec![FileDiffView {
                path: "src/lib.rs".into(),
                change: FileChange::Modified,
                added_lines: 2,
                removed_lines: 1,
                unified: Some("--- a\n+++ b\n".into()),
            }],
        })));
        let taken = draft.take();
        assert_eq!(taken.artifacts, vec!["src/lib.rs"]);
        assert_eq!(taken.diff.as_deref(), Some("--- a\n+++ b\n"));
    }

    /// A completed turn leaves the child addressable; the others close it.
    #[test]
    fn only_a_completed_turn_leaves_a_child_addressable() {
        assert_eq!(agent_state_of(TurnState::Completed), AgentState::Idle);
        assert!(AgentState::Idle.accepts_input());
        assert_eq!(agent_state_of(TurnState::Failed), AgentState::Failed);
        assert_eq!(
            agent_state_of(TurnState::Interrupted),
            AgentState::Interrupted
        );
        assert!(agent_state_of(TurnState::Failed).is_terminal());
    }

    #[test]
    fn a_task_is_bounded_without_splitting_a_character() {
        let task = "é".repeat(MAX_AGENT_TASK + 10);
        assert_eq!(bounded_task(task).chars().count(), MAX_AGENT_TASK);
        assert_eq!(bounded_task("court".into()), "court");
    }
}
