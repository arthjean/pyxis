//! Thread actor and its handle (US-004, US-005, US-007, US-008).
//!
//! One owner per conversation. Clients never touch a `JoinHandle`, a JSONL
//! writer or a cancellation token: they submit to a bounded mailbox, steer the
//! running turn, interrupt it, observe a last-state signal, read a live event
//! stream and ask for a shutdown. The ordering of every operation is decided
//! here, in one task, which is what makes the steer/terminal race arbitrable at
//! all: an input is either consumed by the turn it targeted, or re-admitted as a
//! new turn after that turn's terminal. Never both, never neither.
//!
//! Every limit below is a CONSTANT. FR-20 forbids adding a public configuration
//! key for orchestration in v1.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent::AgentState;
use crate::context::TurnContextSource;
use crate::event::{ThreadEvent, ThreadEventPayload};
use crate::id::{EventId, IdGenerator, ThreadId, TurnId};
use crate::inputs::TurnInputs;
use crate::lifecycle::{TurnLifecycle, TurnState};
use crate::resume::{self, ResumedThread};
use crate::runner::{TurnOutcome, TurnRequest, TurnRunner};
use crate::store::{
    ForkPoint, RecoveryCommit, StoreError, StoreOperation, ThreadSnapshot, ThreadStore,
};
use crate::supervisor::{AgentJournal, AgentSupervisor};

/// Control mailbox depth. A producer that finds it full is refused, never
/// blocked (edge case #2).
pub const COMMAND_MAILBOX: usize = 64;
/// Live event buffer, per subscriber.
pub const LIVE_EVENT_BUFFER: usize = 256;
/// Inputs a thread may hold for one turn: queued turns on one side, steering
/// inputs of the running turn on the other.
pub const MAX_PENDING_INPUTS: usize = 16;
/// Grace given to a cancelled task before it is aborted.
pub const STRAGGLER_ABORT_AFTER: Duration = Duration::from_secs(2);
/// Hard ceiling for a full shutdown, aborts and drain included.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
const MAX_TURN_FAILURE_CAUSE_CHARS: usize = 500;

/// A client input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Submission {
    pub text: String,
    /// Idempotency key. Persisted now, honoured in US-009.
    pub client_message_id: Option<String>,
}

impl Submission {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            client_message_id: None,
        }
    }
}

/// Acknowledgement of an accepted submission. Returned ONLY after its event is
/// durable (FR-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub turn_id: TurnId,
    pub event_id: EventId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStatus {
    pub turn_id: TurnId,
    pub state: TurnState,
}

/// Last known state of a thread. Travels on a `watch`: a client always reads the
/// latest value, never a backlog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStatus {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub health: ThreadHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnStatus>,
    /// Inputs waiting to open a turn of their own.
    pub pending_inputs: usize,
    /// Inputs accepted for the RUNNING turn and not consumed yet (US-007 AC1).
    pub pending_steers: usize,
    pub shutting_down: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ThreadHealth {
    #[default]
    Healthy,
    StoreFailed {
        operation: StoreOperation,
        detail: String,
    },
}

impl ThreadHealth {
    fn is_failed(&self) -> bool {
        matches!(self, Self::StoreFailed { .. })
    }
}

/// Live event published by the runtime.
///
/// Delivered over a `broadcast` rather than an `mpsc`: a stalled or disconnected
/// client must never block the actor, and losing a LIVE event is recoverable
/// because the durable state is the store (edge case #18). A lagging subscriber
/// is told so by `RecvError::Lagged` and re-reads the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub event_id: EventId,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub payload: RuntimeEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RuntimeEventPayload {
    InputAccepted {
        text: String,
    },
    TurnStateChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<TurnState>,
        to: TurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<String>,
    },
    /// Engine event, forwarded with its canonical content untouched.
    Engine(AgentEvent),
    /// A branch was materialized from the turn this event is correlated to.
    Forked {
        child_thread_id: ThreadId,
    },
    /// Live-only signal: the failing writer cannot durably record its own fault.
    StoreFailed {
        operation: StoreOperation,
        detail: String,
    },
    ShuttingDown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("control queue is full")]
    QueueFull,
    #[error("too many pending inputs (max {max})")]
    PendingFull { max: usize },
    /// The turn a steer named is not the turn that is running (US-007 AC5). The
    /// current state travels with the refusal so the client can decide, instead
    /// of silently turning the steer into a new turn.
    #[error("the targeted turn is no longer active")]
    StaleTurn { current: Option<TurnStatus> },
    #[error("thread runtime is shutting down")]
    ShuttingDown,
    #[error("thread runtime stopped")]
    Stopped,
    #[error("thread store failed")]
    StoreFailed,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("thread store {operation} failed during open: {source}")]
    StoreOperation {
        operation: StoreOperation,
        #[source]
        source: StoreError,
    },
}

/// A materialized branch (US-010). Carries its provenance AND its open store,
/// so a client that forks in order to switch to the branch does not have to
/// guess where the runtime put it.
pub struct Fork {
    pub child_thread_id: ThreadId,
    pub fork_turn_id: TurnId,
    /// Terminal event of `fork_turn_id`: the exact cut in the parent's log.
    pub fork_event_id: EventId,
    /// The parent's `Forked` event, durable before this value is handed back.
    pub event_id: EventId,
    pub store: Arc<dyn ThreadStore>,
}

impl std::fmt::Debug for Fork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fork")
            .field("child_thread_id", &self.child_thread_id)
            .field("fork_turn_id", &self.fork_turn_id)
            .field("fork_event_id", &self.fork_event_id)
            .field("event_id", &self.event_id)
            .finish_non_exhaustive()
    }
}

/// Why a branch was refused. Every variant leaves the source untouched: no
/// child file, no event in the parent (US-010 AC5).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForkError {
    #[error("a turn is running: interrupt or finish it before forking")]
    TurnActive { current: TurnStatus },
    #[error("this thread has no terminal turn to fork from")]
    NoTerminalTurn,
    #[error("turn {turn_id} does not belong to this thread")]
    UnknownTurn { turn_id: TurnId },
    #[error("turn {turn_id} is not terminal (`{state}`)")]
    NotTerminal { turn_id: TurnId, state: TurnState },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Submit(#[from] SubmitError),
}

/// Everything a thread needs to run. Assembled by the client (CLI wiring in
/// EP-005), never read from a configuration file.
pub struct ThreadOptions {
    pub thread_id: ThreadId,
    pub store: Arc<dyn ThreadStore>,
    pub runner: Arc<dyn TurnRunner>,
    /// Captures the frozen configuration of each turn (US-006 AC1).
    pub turn_contexts: Arc<dyn TurnContextSource>,
    pub ids: Arc<dyn IdGenerator>,
    pub clock: Arc<dyn Clock>,
    /// Cancellation domain the thread hangs from. The thread takes a CHILD of
    /// it: cancelling the thread never reaches the parent or a sibling.
    pub parent_cancel: CancellationToken,
    /// Sub-agents this thread may own (EP-004). `None` = a thread that spawns
    /// nothing, which is what every child gets: depth stays at 1.
    pub agents: Option<Arc<AgentSupervisor>>,
}

enum Command {
    Submit {
        submission: Submission,
        reply: oneshot::Sender<Result<Accepted, SubmitError>>,
    },
    Steer {
        submission: Submission,
        expected_turn_id: Option<TurnId>,
        reply: oneshot::Sender<Result<Accepted, SubmitError>>,
    },
    Interrupt {
        turn_id: Option<TurnId>,
        reply: oneshot::Sender<Result<Option<TurnStatus>, SubmitError>>,
    },
    Fork {
        at: Option<TurnId>,
        reply: oneshot::Sender<Result<Fork, ForkError>>,
    },
    /// Persists an orchestration event on behalf of something running outside
    /// the actor (EP-004). The actor stays the single writer of its log.
    Record {
        payload: Box<ThreadEventPayload>,
        reply: oneshot::Sender<Result<EventId, SubmitError>>,
    },
}

impl Command {
    /// Answers a command that was never admitted. A refused command must always
    /// answer: a client blocked on a dropped `oneshot` is a hung client.
    fn refuse(self, error: SubmitError) {
        match self {
            Self::Submit { reply, .. } | Self::Steer { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Interrupt { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Record { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Fork { reply, .. } => {
                let _ = reply.send(Err(error.into()));
            }
        }
    }
}

/// [`AgentJournal`] served by the thread's own mailbox.
///
/// A sub-agent tool runs inside a turn task, never inside the actor, so it
/// reaches the durable log the same way a client does: through a bounded
/// mailbox, and with the same refusal when that mailbox is full.
///
/// The sender is WEAK on purpose. The actor owns the supervisor, the supervisor
/// owns this journal: a strong sender here would keep the mailbox alive from
/// inside the actor, and a thread whose every handle was dropped would never
/// reach the "nothing can command me anymore" branch of its own loop.
struct MailboxJournal {
    commands: mpsc::WeakSender<Command>,
}

#[async_trait::async_trait]
impl AgentJournal for MailboxJournal {
    async fn record(&self, payload: ThreadEventPayload) -> Result<EventId, SubmitError> {
        let Some(commands) = self.commands.upgrade() else {
            return Err(SubmitError::Stopped);
        };
        let (reply, answer) = oneshot::channel();
        match commands.try_send(Command::Record {
            payload: Box::new(payload),
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SubmitError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SubmitError::Stopped),
        }
        answer.await.map_err(|_| SubmitError::Stopped)?
    }
}

struct TurnFinished {
    turn_id: TurnId,
    outcome: TurnOutcome,
}

/// The turn the thread is running, with everything needed to steer, interrupt
/// and force-stop it.
struct RunningTurn {
    lifecycle: TurnLifecycle,
    /// Child of the thread token. Cancelling it reaches the engine, its tools
    /// and their process trees, and nothing else.
    token: CancellationToken,
    task: JoinHandle<()>,
    inputs: Arc<TurnInputs>,
}

/// Client-side handle on a running thread.
pub struct ThreadHandle {
    thread_id: ThreadId,
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<ThreadStatus>,
    events: broadcast::Sender<RuntimeEvent>,
    token: CancellationToken,
    actor: Mutex<Option<JoinHandle<()>>>,
    resumed: ResumedThread,
}

impl ThreadHandle {
    /// Opens the durable log, binds it to `thread_id`, replays what it holds and
    /// starts the actor (US-004, US-009).
    ///
    /// A log that already carries turns is RESUMED, not restarted: the last
    /// state, the current turn, the accepted submissions and the fork
    /// provenance come back, and a turn a crash left without a terminal is
    /// closed here, once, before any command is served.
    pub async fn start(options: ThreadOptions) -> Result<Self, RuntimeError> {
        let ThreadOptions {
            thread_id,
            store,
            runner,
            turn_contexts,
            ids,
            clock,
            parent_cancel,
            agents,
        } = options;

        store.create(&thread_id).await?;
        let mut snapshot = store.read().await?;
        let mut plan = resume::plan(thread_id, &snapshot);
        let mut recovered = Vec::new();
        let mut reconciled_calls = 0;
        if !plan.tool_results.is_empty() || !plan.unfinished.is_empty() {
            let next_seq = snapshot.next_seq();
            let repair_id = EventId::generate(ids.as_ref());
            let closures = plan
                .unfinished
                .iter()
                .enumerate()
                .map(|(index, (turn_id, from))| ThreadEvent {
                    event_id: EventId::generate(ids.as_ref()),
                    thread_id,
                    seq: next_seq.saturating_add(index as u64),
                    at_ms: clock.now_ms(),
                    payload: ThreadEventPayload::TurnStateChanged {
                        turn_id: *turn_id,
                        from: Some(*from),
                        to: TurnState::Interrupted,
                        cause: Some(resume::recovery_cause(*from)),
                        context: None,
                    },
                })
                .collect::<Vec<_>>();
            recovered.extend(plan.unfinished.iter().map(|(turn_id, _)| *turn_id));
            reconciled_calls = plan.tool_results.len();
            let repair = RecoveryCommit {
                repair_id,
                thread_id,
                next_seq,
                tool_results: plan.tool_results.clone(),
                closures,
            };
            store.commit_recovery(&repair).await.map_err(|source| {
                RuntimeError::StoreOperation {
                    operation: StoreOperation::CommitRecovery,
                    source,
                }
            })?;
            snapshot = store
                .read()
                .await
                .map_err(|source| RuntimeError::StoreOperation {
                    operation: StoreOperation::Read,
                    source,
                })?;
            plan = resume::plan(thread_id, &snapshot);
        }

        let token = parent_cancel.child_token();
        let (events, _) = broadcast::channel(LIVE_EVENT_BUFFER);
        let (status_tx, status_rx) = watch::channel(ThreadStatus {
            thread_id,
            health: ThreadHealth::Healthy,
            turn: plan.resumed.turn,
            pending_inputs: plan.queued.len(),
            pending_steers: 0,
            shutting_down: false,
        });
        let (command_tx, command_rx) = mpsc::channel(COMMAND_MAILBOX);
        let (finished_tx, finished_rx) = mpsc::channel(COMMAND_MAILBOX);

        let mut actor = ThreadActor {
            thread_id,
            store,
            runner,
            turn_contexts,
            ids,
            clock,
            agents: agents.clone(),
            token: token.clone(),
            tracker: TaskTracker::new(),
            seq: snapshot.next_seq(),
            turn: None,
            last_turn: plan.resumed.turn,
            accepted: plan.resumed.accepted.clone(),
            resolved_runtimes: snapshot
                .events
                .iter()
                .filter_map(|event| match &event.payload {
                    ThreadEventPayload::ModelRuntimeResolved { fingerprint, .. } => {
                        Some(fingerprint.clone())
                    }
                    _ => None,
                })
                .collect(),
            pending: VecDeque::new(),
            straggler_deadline: None,
            events: events.clone(),
            status: status_tx,
            finished_tx,
            finished_rx,
            shutting_down: false,
            health: ThreadHealth::Healthy,
        };
        if !plan.thread_created {
            actor.commit(ThreadEventPayload::ThreadCreated).await?;
        }

        actor.pending = plan.queued.into();
        let mut resumed = plan.resumed;
        resumed.recovered = recovered;
        resumed.reconciled_calls = reconciled_calls;
        if !resumed.recovered.is_empty() || resumed.reconciled_calls > 0 {
            tracing::debug!(
                target: "pyxis::runtime",
                thread_id = %thread_id,
                recovered = resumed.recovered.len(),
                reconciled_calls = resumed.reconciled_calls,
                "turns closed by resume"
            );
        }

        // US-012 AC5: a child the log left open belongs to a process that is
        // gone. Closing it here, once, is what keeps a restart from inheriting
        // a phantom slot.
        for record in &mut resumed.agents {
            if record.state.is_terminal() {
                continue;
            }
            let cause = format!("interrupted: recovered from `{}` at resume", record.state);
            actor
                .commit(ThreadEventPayload::AgentStateChanged {
                    agent_id: record.agent_id,
                    to: AgentState::Interrupted,
                    cause: Some(cause.clone()),
                })
                .await?;
            record.state = AgentState::Interrupted;
            record.cause = Some(cause);
        }

        if let Some(agents) = &agents {
            agents.attach(
                thread_id,
                Arc::new(MailboxJournal {
                    commands: command_tx.downgrade(),
                }),
                token.child_token(),
            );
            agents.restore(resumed.agents.clone());
        }

        actor.start_next_turn().await;
        let join = tokio::spawn(actor.run(command_rx));
        Ok(Self {
            thread_id,
            commands: command_tx,
            status: status_rx,
            events,
            token,
            actor: Mutex::new(Some(join)),
            resumed,
        })
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// What the durable log held when this handle opened it. The transcript a
    /// client chains its next turn on.
    pub fn resumed(&self) -> &ResumedThread {
        &self.resumed
    }

    /// Submits an input that opens a turn of its own.
    ///
    /// Returns once the submission event is durable. Never waits for mailbox
    /// room: a full mailbox is an immediate `QueueFull`, so no identifier is
    /// ever announced as accepted under back-pressure.
    pub async fn submit(&self, submission: Submission) -> Result<Accepted, SubmitError> {
        let (reply, answer) = oneshot::channel();
        self.dispatch(Command::Submit { submission, reply })?;
        answer.await.map_err(|_| SubmitError::Stopped)?
    }

    /// Steers the turn that is running (US-007).
    ///
    /// `expected_turn_id` is the client's claim about what it is correcting.
    /// Naming a turn that is no longer active is refused with the current state:
    /// a correction addressed to a finished turn must not become a new turn
    /// behind the user's back. Passing `None` accepts either branch of the race:
    /// the running turn if there is one, a new turn otherwise.
    pub async fn steer(
        &self,
        submission: Submission,
        expected_turn_id: Option<TurnId>,
    ) -> Result<Accepted, SubmitError> {
        let (reply, answer) = oneshot::channel();
        self.dispatch(Command::Steer {
            submission,
            expected_turn_id,
            reply,
        })?;
        answer.await.map_err(|_| SubmitError::Stopped)?
    }

    /// Interrupts the running turn (US-008).
    ///
    /// Acknowledges as soon as the cancellation is signalled, WITHOUT waiting
    /// for the terminal state: the model, the tools and their process trees stop
    /// cooperatively, and the terminal is written after reconciliation. Returns
    /// the turn that was signalled, or `None` when there was nothing to
    /// interrupt, which is what makes a double interruption a no-op.
    pub async fn interrupt(
        &self,
        turn_id: Option<TurnId>,
    ) -> Result<Option<TurnStatus>, SubmitError> {
        let (reply, answer) = oneshot::channel();
        self.dispatch(Command::Interrupt { turn_id, reply })?;
        answer.await.map_err(|_| SubmitError::Stopped)?
    }

    /// Materializes a branch at a terminal turn boundary (US-010).
    ///
    /// `at` names the turn to branch from; `None` takes the last terminal turn.
    /// Goes through the mailbox like any other operation: the actor is the only
    /// place that knows whether a turn is running, and a fork of a moving
    /// transcript would copy a prefix nobody can reason about.
    pub async fn fork(&self, at: Option<TurnId>) -> Result<Fork, ForkError> {
        let (reply, answer) = oneshot::channel();
        self.dispatch(Command::Fork { at, reply })?;
        answer.await.map_err(|_| SubmitError::Stopped)?
    }

    fn dispatch(&self, command: Command) -> Result<(), SubmitError> {
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SubmitError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::Stopped),
        }
    }

    /// Subscribes to the live event stream. Events emitted before the call are
    /// not replayed: the store is the durable source.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    /// Last known state.
    pub fn status(&self) -> ThreadStatus {
        self.status.borrow().clone()
    }

    /// Watch handle, for a client that wants to await transitions.
    pub fn status_watch(&self) -> watch::Receiver<ThreadStatus> {
        self.status.clone()
    }

    /// Closes admission, cancels and drains the thread, then closes the store.
    ///
    /// Goes through the cancellation token and not through the mailbox: a
    /// shutdown must not queue behind the very commands it is cancelling.
    pub async fn shutdown(&self) {
        self.token.cancel();
        let join = self.actor.lock().ok().and_then(|mut slot| slot.take());
        if let Some(join) = join {
            let _ = join.await;
        }
    }
}

struct ThreadActor {
    thread_id: ThreadId,
    store: Arc<dyn ThreadStore>,
    runner: Arc<dyn TurnRunner>,
    turn_contexts: Arc<dyn TurnContextSource>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    /// Children this thread owns. Held by the actor so a shutdown cancels and
    /// drains them BEFORE the thread writes its own terminal (US-013 AC5).
    agents: Option<Arc<AgentSupervisor>>,
    token: CancellationToken,
    tracker: TaskTracker,
    seq: u64,
    turn: Option<RunningTurn>,
    /// Last turn that reached a terminal state. The slot is freed as soon as the
    /// terminal is durable, but a client still has to be able to read WHICH
    /// state the turn ended in, so the status keeps showing it until the next
    /// turn starts.
    last_turn: Option<TurnStatus>,
    /// Submissions already accepted, by client idempotency key. Rebuilt from the
    /// log at resume, so a retry that crosses a restart is still deduplicated
    /// (FR-12, edge case #3).
    accepted: HashMap<String, Accepted>,
    /// Descriptor bodies already present in the durable log.
    resolved_runtimes: HashSet<String>,
    pending: VecDeque<(TurnId, Submission)>,
    /// Armed when a turn is cancelled. Past it, a turn that ignored the signal
    /// is aborted and the actor writes its terminal itself (US-008 AC5).
    straggler_deadline: Option<Instant>,
    events: broadcast::Sender<RuntimeEvent>,
    status: watch::Sender<ThreadStatus>,
    finished_tx: mpsc::Sender<TurnFinished>,
    finished_rx: mpsc::Receiver<TurnFinished>,
    shutting_down: bool,
    health: ThreadHealth,
}

/// Resolves the cut a fork asks for against the durable log (US-010).
///
/// A boundary is the TERMINAL transition of a turn, and its event identifies the
/// exact prefix to materialize. Naming a turn that is unknown or still open is
/// refused rather than approximated to a nearby boundary.
fn resolve_fork_point(
    snapshot: &ThreadSnapshot,
    at: Option<TurnId>,
) -> Result<(TurnId, EventId), ForkError> {
    let mut last_state: Option<TurnState> = None;
    let mut terminal: Option<(TurnId, EventId)> = None;
    let mut seen = false;

    for event in &snapshot.events {
        let ThreadEventPayload::TurnStateChanged { turn_id, to, .. } = &event.payload else {
            if let (Some(target), Some(turn_id)) = (at, event.turn_id())
                && target == turn_id
            {
                seen = true;
            }
            continue;
        };
        if at.is_some_and(|target| target != *turn_id) {
            continue;
        }
        seen = true;
        last_state = Some(*to);
        if to.is_terminal() {
            terminal = Some((*turn_id, event.event_id));
        }
    }

    match (terminal, at) {
        (Some(point), _) => Ok(point),
        (None, None) => Err(ForkError::NoTerminalTurn),
        (None, Some(turn_id)) if !seen => Err(ForkError::UnknownTurn { turn_id }),
        (None, Some(turn_id)) => Err(ForkError::NotTerminal {
            turn_id,
            state: last_state.unwrap_or(TurnState::Queued),
        }),
    }
}

/// Fires at `deadline`, or never when no turn is being force-stopped.
async fn straggler(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl ThreadActor {
    /// Persists an event then advances the sequence. On failure the sequence
    /// does NOT move: a refused operation leaves no hole in the log.
    async fn commit(&mut self, payload: ThreadEventPayload) -> Result<ThreadEvent, StoreError> {
        if self.health.is_failed() {
            return Err(StoreError::Poisoned);
        }
        let event = ThreadEvent {
            event_id: EventId::generate(self.ids.as_ref()),
            thread_id: self.thread_id,
            seq: self.seq,
            at_ms: self.clock.now_ms(),
            payload,
        };
        if let Err(error) = self.store.append(&event).await {
            self.enter_store_failed(StoreOperation::Append, &error);
            return Err(error);
        }
        self.seq = self.seq.saturating_add(1);
        Ok(event)
    }

    fn enter_store_failed(&mut self, operation: StoreOperation, error: &StoreError) {
        if self.health.is_failed() {
            return;
        }
        let detail = error
            .to_string()
            .chars()
            .take(MAX_TURN_FAILURE_CAUSE_CHARS)
            .collect::<String>();
        self.health = ThreadHealth::StoreFailed {
            operation,
            detail: detail.clone(),
        };
        self.publish(
            EventId::generate(self.ids.as_ref()),
            None,
            RuntimeEventPayload::StoreFailed { operation, detail },
        );
        self.publish_status();
    }

    fn ensure_admission(&self) -> Result<(), SubmitError> {
        if self.health.is_failed() {
            return Err(SubmitError::StoreFailed);
        }
        if self.shutting_down {
            return Err(SubmitError::ShuttingDown);
        }
        Ok(())
    }

    fn publish(&self, event_id: EventId, turn_id: Option<TurnId>, payload: RuntimeEventPayload) {
        let _ = self.events.send(RuntimeEvent {
            event_id,
            thread_id: self.thread_id,
            turn_id,
            payload,
        });
    }

    fn publish_status(&self) {
        self.status.send_replace(ThreadStatus {
            thread_id: self.thread_id,
            health: self.health.clone(),
            turn: self
                .turn
                .as_ref()
                .map(|t| TurnStatus {
                    turn_id: t.lifecycle.turn_id(),
                    state: t.lifecycle.state(),
                })
                .or(self.last_turn),
            pending_inputs: self.pending.len(),
            pending_steers: self.turn.as_ref().map_or(0, |t| t.inputs.len()),
            shutting_down: self.shutting_down,
        });
    }

    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            // Copied out: the `select!` arms may not hold a borrow of `self`
            // while a handler takes it mutably.
            let deadline = self.straggler_deadline;
            tokio::select! {
                // Biased: a terminal always wins over a new command, so the
                // order the actor observes is deterministic under a race, and a
                // turn that landed just before its abort deadline still reports
                // its own outcome.
                biased;
                Some(finished) = self.finished_rx.recv() => {
                    self.on_turn_finished(finished).await;
                }
                () = self.token.cancelled() => break,
                () = straggler(deadline) => {
                    self.on_straggler().await;
                }
                command = commands.recv() => {
                    match command {
                        Some(command) => self.on_command(command).await,
                        // Every handle dropped: nothing can command this thread
                        // anymore.
                        None => break,
                    }
                }
            }
        }
        self.shutdown(&mut commands).await;
    }

    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Submit { submission, reply } => {
                let outcome = self.on_submit(submission).await;
                let _ = reply.send(outcome);
            }
            Command::Steer {
                submission,
                expected_turn_id,
                reply,
            } => {
                let outcome = self.on_steer(submission, expected_turn_id).await;
                let _ = reply.send(outcome);
            }
            Command::Interrupt { turn_id, reply } => {
                let outcome = self.on_interrupt(turn_id);
                let _ = reply.send(outcome);
            }
            Command::Fork { at, reply } => {
                let outcome = self.on_fork(at).await;
                let _ = reply.send(outcome);
            }
            Command::Record { payload, reply } => {
                let outcome = match self.ensure_admission() {
                    Ok(()) => self
                        .commit(*payload)
                        .await
                        .map(|event| event.event_id)
                        .map_err(|_| SubmitError::StoreFailed),
                    Err(error) => Err(error),
                };
                let _ = reply.send(outcome);
            }
        }
    }

    /// Makes an input durable and announces it. The ONLY place an input becomes
    /// visible, so "durable before acknowledgement" holds for a submit and for a
    /// steer alike.
    async fn persist_input(
        &mut self,
        turn_id: TurnId,
        submission: &Submission,
    ) -> Result<EventId, StoreError> {
        let event = self
            .commit(ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id: submission.client_message_id.clone(),
                text: submission.text.clone(),
            })
            .await?;
        if let Some(key) = submission.client_message_id.clone() {
            self.accepted.insert(
                key,
                Accepted {
                    turn_id,
                    event_id: event.event_id,
                },
            );
        }
        self.publish(
            event.event_id,
            Some(turn_id),
            RuntimeEventPayload::InputAccepted {
                text: submission.text.clone(),
            },
        );
        Ok(event.event_id)
    }

    /// Identifiers a previous acceptance of the same `client_message_id` was
    /// given, if any.
    ///
    /// Checked BEFORE admission, shutdown included: replaying a submission the
    /// log already carries is not a new operation, so it can neither be refused
    /// nor executed twice (FR-12).
    fn already_accepted(&self, submission: &Submission) -> Option<Accepted> {
        let key = submission.client_message_id.as_ref()?;
        let prior = self.accepted.get(key).copied();
        if prior.is_some() {
            tracing::debug!(
                target: "pyxis::runtime",
                thread_id = %self.thread_id,
                "submission replayed, returning the original identifiers"
            );
        }
        prior
    }

    async fn on_submit(&mut self, submission: Submission) -> Result<Accepted, SubmitError> {
        if let Some(prior) = self.already_accepted(&submission) {
            return Ok(prior);
        }
        self.ensure_admission()?;
        if self.turn.is_some() && self.pending.len() >= MAX_PENDING_INPUTS {
            return Err(SubmitError::PendingFull {
                max: MAX_PENDING_INPUTS,
            });
        }

        let turn_id = TurnId::generate(self.ids.as_ref());
        let event_id = self
            .persist_input(turn_id, &submission)
            .await
            .map_err(|_| SubmitError::StoreFailed)?;

        // The durable input remains queued until `start_turn` commits Running.
        self.pending.push_back((turn_id, submission));
        self.start_next_turn().await;
        self.publish_status();
        Ok(Accepted { turn_id, event_id })
    }

    async fn on_steer(
        &mut self,
        submission: Submission,
        expected_turn_id: Option<TurnId>,
    ) -> Result<Accepted, SubmitError> {
        if let Some(prior) = self.already_accepted(&submission) {
            return Ok(prior);
        }
        self.ensure_admission()?;

        let Some(running) = self.turn.as_ref() else {
            // The turn reached its terminal before this steer was serialized. A
            // client that named its target learns so; a client that did not gets
            // the other branch of the race, a new queued turn (US-007 AC6).
            if expected_turn_id.is_some() {
                return Err(SubmitError::StaleTurn {
                    current: self.last_turn,
                });
            }
            return self.on_submit(submission).await;
        };

        let active = running.lifecycle.turn_id();
        if let Some(expected) = expected_turn_id
            && expected != active
        {
            return Err(SubmitError::StaleTurn {
                current: Some(TurnStatus {
                    turn_id: active,
                    state: running.lifecycle.state(),
                }),
            });
        }
        if running.inputs.len() >= MAX_PENDING_INPUTS {
            return Err(SubmitError::PendingFull {
                max: MAX_PENDING_INPUTS,
            });
        }
        let inputs = Arc::clone(&running.inputs);

        let event_id = self
            .persist_input(active, &submission)
            .await
            .map_err(|_| SubmitError::StoreFailed)?;
        // Queued only AFTER the event is durable: the engine must never consume
        // an input the log does not carry.
        inputs.push(submission.text);
        self.publish_status();
        Ok(Accepted {
            turn_id: active,
            event_id,
        })
    }

    /// Signals the running turn. Answers without awaiting anything: the whole
    /// point of US-008 AC2 is that an interruption is acknowledged in the time
    /// it takes to flip a token, not in the time it takes to stop a model.
    fn on_interrupt(&mut self, turn_id: Option<TurnId>) -> Result<Option<TurnStatus>, SubmitError> {
        let Some(running) = self.turn.as_ref() else {
            return Ok(None);
        };
        let active = running.lifecycle.turn_id();
        if let Some(target) = turn_id
            && target != active
        {
            return Ok(None);
        }

        running.token.cancel();
        // Rearming on a second interruption would push the abort further away,
        // so the deadline of the first signal is the one that counts.
        self.straggler_deadline
            .get_or_insert_with(|| Instant::now() + STRAGGLER_ABORT_AFTER);
        tracing::debug!(
            target: "pyxis::runtime",
            thread_id = %self.thread_id,
            turn_id = %active,
            "turn interruption signalled"
        );
        Ok(Some(TurnStatus {
            turn_id: active,
            state: running.lifecycle.state(),
        }))
    }

    /// Cuts a branch at a terminal turn boundary (US-010).
    ///
    /// Order matters: refuse, resolve against the DURABLE log, materialize, and
    /// only then record the fork in the parent. A failure at any step leaves the
    /// parent exactly as it was.
    async fn on_fork(&mut self, at: Option<TurnId>) -> Result<Fork, ForkError> {
        self.ensure_admission()?;
        if let Some(running) = self.turn.as_ref() {
            // Edge case #12: a moving transcript has no boundary to copy.
            return Err(ForkError::TurnActive {
                current: TurnStatus {
                    turn_id: running.lifecycle.turn_id(),
                    state: running.lifecycle.state(),
                },
            });
        }

        // Read back rather than trust in-memory state: the cut is a byte offset
        // in the log, so the log is what has to name it.
        let snapshot = match self.store.read().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.enter_store_failed(StoreOperation::Read, &error);
                return Err(SubmitError::StoreFailed.into());
            }
        };
        let (fork_turn_id, fork_event_id) = resolve_fork_point(&snapshot, at)?;
        let child_thread_id = ThreadId::generate(self.ids.as_ref());
        let point = ForkPoint {
            parent_thread_id: self.thread_id,
            child_thread_id,
            fork_turn_id,
            fork_event_id,
        };
        let store = match self.store.fork(&point).await {
            Ok(store) => store,
            Err(error) => {
                self.enter_store_failed(StoreOperation::Fork, &error);
                return Err(SubmitError::StoreFailed.into());
            }
        };

        let event = match self
            .commit(ThreadEventPayload::Forked {
                child_thread_id,
                fork_turn_id,
                fork_event_id,
            })
            .await
        {
            Ok(event) => event,
            Err(err) => {
                // The branch is on disk and self-describing: it carries its own
                // origin, so nothing about it is partial. What failed is the
                // parent's record of it, and naming it here is what keeps it
                // from becoming an anonymous file.
                tracing::warn!(
                    thread_id = %self.thread_id,
                    child_thread_id = %child_thread_id,
                    error = %err,
                    "branch materialized but its parent could not record it"
                );
                return Err(SubmitError::StoreFailed.into());
            }
        };
        self.publish(
            event.event_id,
            Some(fork_turn_id),
            RuntimeEventPayload::Forked { child_thread_id },
        );
        tracing::debug!(
            target: "pyxis::runtime",
            thread_id = %self.thread_id,
            child_thread_id = %child_thread_id,
            fork_turn_id = %fork_turn_id,
            "branch materialized"
        );
        Ok(Fork {
            child_thread_id,
            fork_turn_id,
            fork_event_id,
            event_id: event.event_id,
            store,
        })
    }

    /// Promotes queued inputs in order. A store fault leaves the front item in
    /// place, so a healthy reopen can start the accepted turn exactly once.
    async fn start_next_turn(&mut self) {
        while self.turn.is_none() && !self.health.is_failed() {
            let Some((turn_id, submission)) = self.pending.front().cloned() else {
                break;
            };
            match self.start_turn(turn_id, &submission).await {
                Ok(()) => {
                    self.pending.pop_front();
                    if self.turn.is_some() {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        thread_id = %self.thread_id,
                        turn_id = %turn_id,
                        error = %error,
                        "queued turn could not be promoted"
                    );
                    break;
                }
            }
        }
        self.publish_status();
    }

    async fn start_turn(
        &mut self,
        turn_id: TurnId,
        submission: &Submission,
    ) -> Result<(), StoreError> {
        if self.shutting_down {
            return Ok(());
        }
        // US-006 AC1: captured ONCE, before the transition that starts the turn
        // is written, so the log records the configuration the engine actually
        // received. What the source does afterwards belongs to the next turn.
        let captured = match self.turn_contexts.capture(turn_id).and_then(|captured| {
            if let Some(runtime) = &captured.model_runtime {
                runtime
                    .validate()
                    .map_err(|error| crate::context::TurnContextError(error.to_string()))?;
                if captured.context.model != runtime.slug {
                    return Err(crate::context::TurnContextError(
                        "captured model does not match resolved runtime".into(),
                    ));
                }
                if captured.context.reasoning_effort != runtime.reasoning_effort {
                    return Err(crate::context::TurnContextError(
                        "captured reasoning effort does not match resolved runtime".into(),
                    ));
                }
                if captured.context.limits.max_output_tokens != runtime.max_output_tokens {
                    return Err(crate::context::TurnContextError(
                        "captured output limit does not match resolved runtime".into(),
                    ));
                }
                if captured.context.model_runtime_fingerprint.as_deref()
                    != Some(runtime.fingerprint.as_str())
                {
                    return Err(crate::context::TurnContextError(
                        "captured fingerprint does not match resolved runtime".into(),
                    ));
                }
            } else if captured.context.model_runtime_fingerprint.is_some() {
                return Err(crate::context::TurnContextError(
                    "captured fingerprint has no resolved runtime".into(),
                ));
            }
            Ok(captured)
        }) {
            Ok(captured) => captured,
            Err(error) => {
                let cause = format!("model runtime refused: {error}")
                    .chars()
                    .take(MAX_TURN_FAILURE_CAUSE_CHARS)
                    .collect::<String>();
                match self
                    .commit(ThreadEventPayload::TurnStateChanged {
                        turn_id,
                        from: Some(TurnState::Queued),
                        to: TurnState::Failed,
                        cause: Some(cause.clone()),
                        context: None,
                    })
                    .await
                {
                    Ok(event) => {
                        self.last_turn = Some(TurnStatus {
                            turn_id,
                            state: TurnState::Failed,
                        });
                        self.publish(
                            event.event_id,
                            Some(turn_id),
                            RuntimeEventPayload::TurnStateChanged {
                                from: Some(TurnState::Queued),
                                to: TurnState::Failed,
                                cause: Some(cause),
                            },
                        );
                    }
                    Err(store_error) => return Err(store_error),
                }
                self.publish_status();
                return Ok(());
            }
        };
        let context = captured.context;
        let model_runtime = captured.model_runtime;
        if let Some(runtime) = &model_runtime
            && !self.resolved_runtimes.contains(&runtime.fingerprint)
        {
            let fingerprint = runtime.fingerprint.clone();
            if let Err(error) = self
                .commit(ThreadEventPayload::ModelRuntimeResolved {
                    fingerprint: fingerprint.clone(),
                    runtime: runtime.clone(),
                })
                .await
            {
                tracing::warn!(
                    thread_id = %self.thread_id,
                    turn_id = %turn_id,
                    error = %error,
                    "model runtime descriptor not persisted"
                );
                return Err(error);
            }
            self.resolved_runtimes.insert(fingerprint);
        }
        let mut lifecycle = TurnLifecycle::queued(turn_id);
        let Ok(from) = lifecycle.transition(TurnState::Running) else {
            return Ok(());
        };
        // US-005 AC1: `running` is durable BEFORE the engine exists, so no
        // provider call can happen for a turn the log never started.
        let event = match self
            .commit(ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(from),
                to: TurnState::Running,
                cause: None,
                context: Some(context.clone()),
            })
            .await
        {
            Ok(event) => event,
            Err(err) => {
                // The input remains queued and admission closes until reopen.
                tracing::warn!(thread_id = %self.thread_id, turn_id = %turn_id, error = %err, "turn start not persisted");
                return Err(err);
            }
        };
        self.publish(
            event.event_id,
            Some(turn_id),
            RuntimeEventPayload::TurnStateChanged {
                from: Some(from),
                to: TurnState::Running,
                cause: None,
            },
        );

        let inputs = Arc::new(TurnInputs::new());
        let (engine_tx, mut engine_rx) = mpsc::channel::<AgentEvent>(LIVE_EVENT_BUFFER);
        let turn_token = self.token.child_token();
        let runner = Arc::clone(&self.runner);
        let ids = Arc::clone(&self.ids);
        let events = self.events.clone();
        let finished_tx = self.finished_tx.clone();
        let thread_id = self.thread_id;
        let request = TurnRequest {
            turn_id,
            text: submission.text.clone(),
            context,
            model_runtime,
            inputs: Arc::clone(&inputs),
        };

        let task = self.tracker.spawn({
            let turn_token = turn_token.clone();
            async move {
                let forward = async {
                    while let Some(event) = engine_rx.recv().await {
                        // US-005 AC2: correlated, never rewritten.
                        let _ = events.send(RuntimeEvent {
                            event_id: EventId::generate(ids.as_ref()),
                            thread_id,
                            turn_id: Some(turn_id),
                            payload: RuntimeEventPayload::Engine(event),
                        });
                    }
                };
                let (outcome, ()) =
                    tokio::join!(runner.run_turn(request, engine_tx, turn_token), forward);
                let _ = finished_tx.send(TurnFinished { turn_id, outcome }).await;
            }
        });

        self.turn = Some(RunningTurn {
            lifecycle,
            token: turn_token,
            task,
            inputs,
        });
        self.publish_status();
        Ok(())
    }

    async fn on_turn_finished(&mut self, finished: TurnFinished) {
        let Some(running) = self.turn.as_mut() else {
            return;
        };
        if running.lifecycle.turn_id() != finished.turn_id {
            return;
        }
        let to = finished.outcome.terminal_state();
        let cause = finished.outcome.cause();
        let mut terminal = running.lifecycle.clone();
        let from = match terminal.transition(to) {
            Ok(from) => from,
            Err(err) => {
                // A terminal was already written: refusing here is what makes a
                // repeated terminal idempotent instead of duplicating it.
                tracing::warn!(thread_id = %self.thread_id, error = %err, "refused a second terminal");
                return;
            }
        };
        if self
            .record_terminal(finished.turn_id, from, to, cause)
            .await
        {
            if let Some(running) = self.turn.as_mut() {
                running.lifecycle = terminal;
            }
            if let Some(running) = self.turn.take() {
                self.release_turn(running).await;
            }
        }
    }

    /// A cancelled turn ignored its signal for longer than the grace period.
    /// Abort it, wait for it to actually unwind, write the terminal it owes and
    /// free the slot (US-008 AC5).
    async fn on_straggler(&mut self) {
        self.straggler_deadline = None;
        let Some(running) = self.turn.as_ref() else {
            return;
        };
        if running.task.is_finished() {
            // It landed inside the grace period; its own outcome is on its way
            // through `finished_rx` and must win.
            return;
        }

        let Some(mut running) = self.turn.take() else {
            return;
        };
        let turn_id = running.lifecycle.turn_id();
        tracing::warn!(
            thread_id = %self.thread_id,
            turn_id = %turn_id,
            "turn ignored cancellation for {STRAGGLER_ABORT_AFTER:?}, aborting"
        );
        running.task.abort();
        // Drained BEFORE the terminal: unwinding is what runs the destructors of
        // the tools, and those are what terminate the process trees a shell
        // command started (AC4). Writing `interrupted` first would announce a
        // stop that had not happened yet. Bounded so the whole force-stop stays
        // inside the shutdown budget.
        let drain = SHUTDOWN_DEADLINE.saturating_sub(STRAGGLER_ABORT_AFTER);
        if tokio::time::timeout(drain, &mut running.task)
            .await
            .is_err()
        {
            tracing::warn!(
                thread_id = %self.thread_id,
                turn_id = %turn_id,
                "aborted turn did not unwind inside {drain:?}"
            );
        }

        let mut terminal = running.lifecycle.clone();
        let committed = if let Ok(from) = terminal.transition(TurnState::Interrupted) {
            self.record_terminal(
                turn_id,
                from,
                TurnState::Interrupted,
                Some("interrupted: task aborted".into()),
            )
            .await
        } else {
            false
        };
        if committed {
            running.lifecycle = terminal;
            self.release_turn(running).await;
        } else {
            self.turn = Some(running);
            self.publish_status();
        }
    }

    /// Frees the turn slot after its terminal was written: disarm the abort
    /// deadline, re-admit the inputs the engine never consumed, then start the
    /// next queued turn.
    async fn release_turn(&mut self, running: RunningTurn) {
        self.straggler_deadline = None;

        if !self.shutting_down {
            // US-007 AC6, second branch: a steer accepted for a turn that
            // reached its terminal before consuming it is not lost, it opens a
            // turn of its own. It carries a new `TurnId` because the one it was
            // accepted under is terminal, and the engine takes an input exactly
            // once, so nothing is consumed twice.
            for text in running.inputs.drain_remaining() {
                let submission = Submission::new(text);
                let turn_id = TurnId::generate(self.ids.as_ref());
                match self.persist_input(turn_id, &submission).await {
                    Ok(_) => self.pending.push_back((turn_id, submission)),
                    Err(err) => {
                        tracing::warn!(thread_id = %self.thread_id, error = %err, "unconsumed input not re-admitted");
                        break;
                    }
                }
            }
            self.start_next_turn().await;
        }
        self.publish_status();
    }

    async fn record_terminal(
        &mut self,
        turn_id: TurnId,
        from: TurnState,
        to: TurnState,
        cause: Option<String>,
    ) -> bool {
        match self
            .commit(ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(from),
                to,
                cause: cause.clone(),
                context: None,
            })
            .await
        {
            Ok(event) => {
                self.last_turn = Some(TurnStatus { turn_id, state: to });
                self.publish(
                    event.event_id,
                    Some(turn_id),
                    RuntimeEventPayload::TurnStateChanged {
                        from: Some(from),
                        to,
                        cause,
                    },
                );
                self.publish_status();
                true
            }
            Err(err) => {
                tracing::warn!(thread_id = %self.thread_id, turn_id = %turn_id, error = %err, "terminal state not persisted");
                false
            }
        }
    }

    /// Close admission, cancel, wait, abort the stragglers, drain, then close
    /// the store. In that order: the store must outlive every task that could
    /// still want to append.
    async fn shutdown(&mut self, commands: &mut mpsc::Receiver<Command>) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.publish(
            EventId::generate(self.ids.as_ref()),
            None,
            RuntimeEventPayload::ShuttingDown,
        );
        self.publish_status();

        commands.close();
        while let Ok(command) = commands.try_recv() {
            command.refuse(SubmitError::ShuttingDown);
        }

        self.token.cancel();
        self.tracker.close();
        let mut aborted = false;
        // US-013 AC5: the children hang from a node of this thread's token, so
        // the cancel above already reached them. They are drained HERE, next to
        // the thread's own tasks and before any terminal is written, so no
        // sub-agent outlives the parent that owns it.
        let children = async {
            if let Some(agents) = &self.agents {
                agents.shutdown().await;
            }
        };
        if tokio::time::timeout(STRAGGLER_ABORT_AFTER, async {
            tokio::join!(self.tracker.wait(), children);
        })
        .await
        .is_err()
        {
            aborted = true;
            tracing::warn!(
                thread_id = %self.thread_id,
                "turn task ignored cancellation for {STRAGGLER_ABORT_AFTER:?}, aborting"
            );
            if let Some(running) = &self.turn {
                running.task.abort();
            }
            let _ = tokio::time::timeout(
                SHUTDOWN_DEADLINE.saturating_sub(STRAGGLER_ABORT_AFTER),
                self.tracker.wait(),
            )
            .await;
        }

        // Children closed while the mailbox was shutting down could not go
        // through it. The actor is still the writer, so it persists them here
        // rather than leaving the next resume to repair the graph.
        if let Some(agents) = self.agents.clone() {
            for payload in agents.take_unrecorded() {
                if let Err(err) = self.commit(payload).await {
                    tracing::warn!(thread_id = %self.thread_id, error = %err, "sub-agent terminal not persisted");
                }
            }
        }

        // Terminals produced while cancelling are still owed to the log.
        self.finished_rx.close();
        while let Ok(finished) = self.finished_rx.try_recv() {
            self.on_turn_finished(finished).await;
        }
        // A turn still alive here was force-stopped and owes its single terminal.
        if !self.health.is_failed()
            && let Some(running) = self.turn.as_ref()
        {
            let turn_id = running.lifecycle.turn_id();
            let mut terminal = running.lifecycle.clone();
            if let Ok(from) = terminal.transition(TurnState::Interrupted) {
                let cause = if aborted {
                    "shutdown: task aborted"
                } else {
                    "shutdown"
                };
                if self
                    .record_terminal(turn_id, from, TurnState::Interrupted, Some(cause.into()))
                    .await
                    && let Some(running) = self.turn.as_mut()
                {
                    running.lifecycle = terminal;
                }
            }
        }
        self.turn = None;

        self.pending.clear();
        self.publish_status();
        if let Err(err) = self.store.close().await {
            tracing::warn!(thread_id = %self.thread_id, error = %err, "thread store close failed");
            self.enter_store_failed(StoreOperation::Close, &err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SequentialIds;

    fn log(ids: &dyn IdGenerator, payloads: Vec<ThreadEventPayload>) -> ThreadSnapshot {
        let thread_id = ThreadId::generate(ids);
        ThreadSnapshot {
            thread_id: Some(thread_id),
            events: payloads
                .into_iter()
                .enumerate()
                .map(|(i, payload)| ThreadEvent {
                    event_id: EventId::generate(ids),
                    thread_id,
                    seq: i as u64 + 1,
                    at_ms: i as u64,
                    payload,
                })
                .collect(),
            ..ThreadSnapshot::default()
        }
    }

    /// A turn that is queued but not running is unreachable from a live actor
    /// (a fork is refused before it while a turn is active) and impossible after
    /// a resume (every open turn is closed there). The refusal is still typed:
    /// approximating it to the nearest boundary would branch a transcript the
    /// caller never named.
    #[test]
    fn a_fork_at_a_turn_that_never_reached_a_terminal_is_refused() {
        let ids = SequentialIds::new();
        let queued = TurnId::generate(&ids);
        let snapshot = log(
            &ids,
            vec![
                ThreadEventPayload::ThreadCreated,
                ThreadEventPayload::InputSubmitted {
                    turn_id: queued,
                    client_message_id: None,
                    text: "en attente".into(),
                },
                ThreadEventPayload::TurnStateChanged {
                    turn_id: queued,
                    from: Some(TurnState::Queued),
                    to: TurnState::Running,
                    cause: None,
                    context: None,
                },
            ],
        );
        assert!(matches!(
            resolve_fork_point(&snapshot, Some(queued)),
            Err(ForkError::NotTerminal {
                state: TurnState::Running,
                ..
            })
        ));
        assert!(matches!(
            resolve_fork_point(&snapshot, None),
            Err(ForkError::NoTerminalTurn)
        ));
    }

    /// Without a target, the LAST terminal wins; with one, its own terminal does.
    #[test]
    fn a_fork_point_resolves_to_the_terminal_of_the_named_turn() {
        let ids = SequentialIds::new();
        let first = TurnId::generate(&ids);
        let second = TurnId::generate(&ids);
        let snapshot = log(
            &ids,
            vec![
                ThreadEventPayload::TurnStateChanged {
                    turn_id: first,
                    from: Some(TurnState::Running),
                    to: TurnState::Completed,
                    cause: None,
                    context: None,
                },
                ThreadEventPayload::TurnStateChanged {
                    turn_id: second,
                    from: Some(TurnState::Running),
                    to: TurnState::Interrupted,
                    cause: None,
                    context: None,
                },
            ],
        );
        assert_eq!(
            resolve_fork_point(&snapshot, None).unwrap(),
            (second, snapshot.events[1].event_id)
        );
        assert_eq!(
            resolve_fork_point(&snapshot, Some(first)).unwrap(),
            (first, snapshot.events[0].event_id)
        );
        // An interrupted turn is a boundary too: it is terminal.
        assert_eq!(
            resolve_fork_point(&snapshot, Some(second)).unwrap().0,
            second
        );
    }
}
