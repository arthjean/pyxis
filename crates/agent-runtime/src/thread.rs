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

use std::collections::VecDeque;
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

use crate::context::TurnContextSource;
use crate::event::{ThreadEvent, ThreadEventPayload};
use crate::id::{EventId, IdGenerator, ThreadId, TurnId};
use crate::inputs::TurnInputs;
use crate::lifecycle::{TurnLifecycle, TurnState};
use crate::runner::{TurnOutcome, TurnRequest, TurnRunner};
use crate::store::{StoreError, ThreadStore};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnStatus>,
    /// Inputs waiting to open a turn of their own.
    pub pending_inputs: usize,
    /// Inputs accepted for the RUNNING turn and not consumed yet (US-007 AC1).
    pub pending_steers: usize,
    pub shutting_down: bool,
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
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
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
        }
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
}

impl ThreadHandle {
    /// Opens the durable log, binds it to `thread_id` and starts the actor.
    pub async fn start(options: ThreadOptions) -> Result<Self, RuntimeError> {
        let ThreadOptions {
            thread_id,
            store,
            runner,
            turn_contexts,
            ids,
            clock,
            parent_cancel,
        } = options;

        store.create(&thread_id).await?;
        let snapshot = store.read().await?;
        let already_created = snapshot
            .events
            .iter()
            .any(|e| matches!(e.payload, ThreadEventPayload::ThreadCreated));

        let token = parent_cancel.child_token();
        let (events, _) = broadcast::channel(LIVE_EVENT_BUFFER);
        let (status_tx, status_rx) = watch::channel(ThreadStatus {
            thread_id,
            turn: None,
            pending_inputs: 0,
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
            token: token.clone(),
            tracker: TaskTracker::new(),
            seq: snapshot.next_seq(),
            turn: None,
            last_turn: None,
            pending: VecDeque::new(),
            straggler_deadline: None,
            events: events.clone(),
            status: status_tx,
            finished_tx,
            finished_rx,
            shutting_down: false,
        };
        if !already_created {
            actor.commit(ThreadEventPayload::ThreadCreated).await?;
        }

        let join = tokio::spawn(actor.run(command_rx));
        Ok(Self {
            thread_id,
            commands: command_tx,
            status: status_rx,
            events,
            token,
            actor: Mutex::new(Some(join)),
        })
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
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
    token: CancellationToken,
    tracker: TaskTracker,
    seq: u64,
    turn: Option<RunningTurn>,
    /// Last turn that reached a terminal state. The slot is freed as soon as the
    /// terminal is durable, but a client still has to be able to read WHICH
    /// state the turn ended in, so the status keeps showing it until the next
    /// turn starts.
    last_turn: Option<TurnStatus>,
    pending: VecDeque<(TurnId, Submission)>,
    /// Armed when a turn is cancelled. Past it, a turn that ignored the signal
    /// is aborted and the actor writes its terminal itself (US-008 AC5).
    straggler_deadline: Option<Instant>,
    events: broadcast::Sender<RuntimeEvent>,
    status: watch::Sender<ThreadStatus>,
    finished_tx: mpsc::Sender<TurnFinished>,
    finished_rx: mpsc::Receiver<TurnFinished>,
    shutting_down: bool,
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
        let event = ThreadEvent {
            event_id: EventId::generate(self.ids.as_ref()),
            thread_id: self.thread_id,
            seq: self.seq,
            at_ms: self.clock.now_ms(),
            payload,
        };
        self.store.append(&event).await?;
        self.seq = self.seq.saturating_add(1);
        Ok(event)
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
        self.publish(
            event.event_id,
            Some(turn_id),
            RuntimeEventPayload::InputAccepted {
                text: submission.text.clone(),
            },
        );
        Ok(event.event_id)
    }

    async fn on_submit(&mut self, submission: Submission) -> Result<Accepted, SubmitError> {
        if self.shutting_down {
            return Err(SubmitError::ShuttingDown);
        }
        if self.turn.is_some() && self.pending.len() >= MAX_PENDING_INPUTS {
            return Err(SubmitError::PendingFull {
                max: MAX_PENDING_INPUTS,
            });
        }

        let turn_id = TurnId::generate(self.ids.as_ref());
        let event_id = self.persist_input(turn_id, &submission).await?;

        if self.turn.is_some() {
            // FR-02: a single regular turn at a time. The input stays queued
            // until the active turn reaches its terminal.
            self.pending.push_back((turn_id, submission));
        } else {
            self.start_turn(turn_id, submission).await;
        }
        self.publish_status();
        Ok(Accepted { turn_id, event_id })
    }

    async fn on_steer(
        &mut self,
        submission: Submission,
        expected_turn_id: Option<TurnId>,
    ) -> Result<Accepted, SubmitError> {
        if self.shutting_down {
            return Err(SubmitError::ShuttingDown);
        }

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

        let event_id = self.persist_input(active, &submission).await?;
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

    async fn start_turn(&mut self, turn_id: TurnId, submission: Submission) {
        if self.shutting_down {
            return;
        }
        let mut lifecycle = TurnLifecycle::queued(turn_id);
        let Ok(from) = lifecycle.transition(TurnState::Running) else {
            return;
        };
        // US-005 AC1: `running` is durable BEFORE the engine exists, so no
        // provider call can happen for a turn the log never started.
        let event = match self
            .commit(ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(from),
                to: TurnState::Running,
                cause: None,
            })
            .await
        {
            Ok(event) => event,
            Err(err) => {
                // The store refused: no engine starts, because its terminal
                // could not be recorded either. The thread stays commandable.
                tracing::warn!(thread_id = %self.thread_id, turn_id = %turn_id, error = %err, "turn start not persisted");
                return;
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

        // US-006 AC1: captured ONCE. What the source does afterwards belongs to
        // the next turn.
        let context = self.turn_contexts.capture(turn_id);
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
            text: submission.text,
            context,
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
        let from = match running.lifecycle.transition(to) {
            Ok(from) => from,
            Err(err) => {
                // A terminal was already written: refusing here is what makes a
                // repeated terminal idempotent instead of duplicating it.
                tracing::warn!(thread_id = %self.thread_id, error = %err, "refused a second terminal");
                return;
            }
        };
        self.record_terminal(finished.turn_id, from, to, cause)
            .await;
        if let Some(running) = self.turn.take() {
            self.release_turn(running).await;
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

        if let Ok(from) = running.lifecycle.transition(TurnState::Interrupted) {
            self.record_terminal(
                turn_id,
                from,
                TurnState::Interrupted,
                Some("interrupted: task aborted".into()),
            )
            .await;
        }
        self.release_turn(running).await;
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
                    }
                }
            }
            if let Some((turn_id, submission)) = self.pending.pop_front() {
                self.start_turn(turn_id, submission).await;
            }
        }
        self.publish_status();
    }

    async fn record_terminal(
        &mut self,
        turn_id: TurnId,
        from: TurnState,
        to: TurnState,
        cause: Option<String>,
    ) {
        self.last_turn = Some(TurnStatus { turn_id, state: to });
        match self
            .commit(ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(from),
                to,
                cause: cause.clone(),
            })
            .await
        {
            Ok(event) => self.publish(
                event.event_id,
                Some(turn_id),
                RuntimeEventPayload::TurnStateChanged {
                    from: Some(from),
                    to,
                    cause,
                },
            ),
            Err(err) => {
                tracing::warn!(thread_id = %self.thread_id, turn_id = %turn_id, error = %err, "terminal state not persisted");
                self.publish(
                    EventId::generate(self.ids.as_ref()),
                    Some(turn_id),
                    RuntimeEventPayload::TurnStateChanged {
                        from: Some(from),
                        to,
                        cause,
                    },
                );
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
        if tokio::time::timeout(STRAGGLER_ABORT_AFTER, self.tracker.wait())
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

        // Terminals produced while cancelling are still owed to the log.
        self.finished_rx.close();
        while let Ok(finished) = self.finished_rx.try_recv() {
            self.on_turn_finished(finished).await;
        }
        // A turn still alive here was force-stopped and owes its single terminal.
        if let Some(running) = self.turn.as_mut() {
            let turn_id = running.lifecycle.turn_id();
            if let Ok(from) = running.lifecycle.transition(TurnState::Interrupted) {
                let cause = if aborted {
                    "shutdown: task aborted"
                } else {
                    "shutdown"
                };
                self.record_terminal(turn_id, from, TurnState::Interrupted, Some(cause.into()))
                    .await;
            }
        }
        self.turn = None;

        self.pending.clear();
        self.publish_status();
        if let Err(err) = self.store.close().await {
            tracing::warn!(thread_id = %self.thread_id, error = %err, "thread store close failed");
        }
    }
}
