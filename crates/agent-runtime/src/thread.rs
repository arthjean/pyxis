//! Thread actor and its handle (US-004).
//!
//! One owner per conversation. Clients never touch a `JoinHandle`, a JSONL
//! writer or a cancellation token: they submit to a bounded mailbox, observe a
//! last-state signal, read a live event stream and ask for a shutdown. The
//! ordering of every operation is decided here, in one task, which is what makes
//! the steer/terminal races of EP-002 arbitrable at all.
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
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::event::{ThreadEvent, ThreadEventPayload};
use crate::id::{EventId, IdGenerator, ThreadId, TurnId};
use crate::lifecycle::{TurnLifecycle, TurnState};
use crate::runner::{TurnOutcome, TurnRequest, TurnRunner};
use crate::store::{StoreError, ThreadStore};

/// Control mailbox depth. A producer that finds it full is refused, never
/// blocked (edge case #2).
pub const COMMAND_MAILBOX: usize = 64;
/// Live event buffer, per subscriber.
pub const LIVE_EVENT_BUFFER: usize = 256;
/// Inputs a thread may hold while a turn is running.
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
    pub pending_inputs: usize,
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
}

struct TurnFinished {
    turn_id: TurnId,
    outcome: TurnOutcome,
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
            shutting_down: false,
        });
        let (command_tx, command_rx) = mpsc::channel(COMMAND_MAILBOX);
        let (finished_tx, finished_rx) = mpsc::channel(COMMAND_MAILBOX);

        let mut actor = ThreadActor {
            thread_id,
            store,
            runner,
            ids,
            clock,
            token: token.clone(),
            tracker: TaskTracker::new(),
            turn_tasks: Vec::new(),
            seq: snapshot.next_seq(),
            turn: None,
            pending: VecDeque::new(),
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

    /// Submits an input. Returns once the submission event is durable.
    ///
    /// Never waits for mailbox room: a full mailbox is an immediate `QueueFull`,
    /// so no identifier is ever announced as accepted under back-pressure.
    pub async fn submit(&self, submission: Submission) -> Result<Accepted, SubmitError> {
        let (reply, answer) = oneshot::channel();
        match self
            .commands
            .try_send(Command::Submit { submission, reply })
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SubmitError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SubmitError::Stopped),
        }
        answer.await.map_err(|_| SubmitError::Stopped)?
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
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    token: CancellationToken,
    tracker: TaskTracker,
    turn_tasks: Vec<JoinHandle<()>>,
    seq: u64,
    turn: Option<TurnLifecycle>,
    pending: VecDeque<(TurnId, Submission)>,
    events: broadcast::Sender<RuntimeEvent>,
    status: watch::Sender<ThreadStatus>,
    finished_tx: mpsc::Sender<TurnFinished>,
    finished_rx: mpsc::Receiver<TurnFinished>,
    shutting_down: bool,
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
            turn: self.turn.as_ref().map(|t| TurnStatus {
                turn_id: t.turn_id(),
                state: t.state(),
            }),
            pending_inputs: self.pending.len(),
            shutting_down: self.shutting_down,
        });
    }

    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            tokio::select! {
                // Biased: a terminal always wins over a new command, so the
                // order the actor observes is deterministic under a race.
                biased;
                Some(finished) = self.finished_rx.recv() => {
                    self.on_turn_finished(finished).await;
                }
                () = self.token.cancelled() => break,
                command = commands.recv() => {
                    match command {
                        Some(Command::Submit { submission, reply }) => {
                            let outcome = self.on_submit(submission).await;
                            let _ = reply.send(outcome);
                        }
                        // Every handle dropped: nothing can command this thread
                        // anymore.
                        None => break,
                    }
                }
            }
        }
        self.shutdown(&mut commands).await;
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

        if self.turn.is_some() {
            // FR-02: a single regular turn at a time. The input stays queued
            // until the active turn reaches its terminal.
            self.pending.push_back((turn_id, submission));
        } else {
            self.start_turn(turn_id, submission).await;
        }
        self.publish_status();
        Ok(Accepted {
            turn_id,
            event_id: event.event_id,
        })
    }

    async fn start_turn(&mut self, turn_id: TurnId, submission: Submission) {
        if self.shutting_down {
            return;
        }
        let mut lifecycle = TurnLifecycle::queued(turn_id);
        let Ok(from) = lifecycle.transition(TurnState::Running) else {
            return;
        };
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
        };

        let task = self.tracker.spawn(async move {
            let forward = async {
                while let Some(event) = engine_rx.recv().await {
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
        });

        self.turn_tasks.push(task);
        self.turn = Some(lifecycle);
        self.publish_status();
    }

    async fn on_turn_finished(&mut self, finished: TurnFinished) {
        let Some(mut lifecycle) = self.turn.take() else {
            return;
        };
        if lifecycle.turn_id() != finished.turn_id {
            self.turn = Some(lifecycle);
            return;
        }
        let to = finished.outcome.terminal_state();
        let cause = finished.outcome.cause();
        let from = match lifecycle.transition(to) {
            Ok(from) => from,
            Err(err) => {
                // A terminal was already written: refusing here is what makes a
                // repeated terminal idempotent instead of duplicating it.
                tracing::warn!(thread_id = %self.thread_id, error = %err, "refused a second terminal");
                self.turn = Some(lifecycle);
                return;
            }
        };
        self.record_terminal(finished.turn_id, from, to, cause)
            .await;

        self.turn_tasks.retain(|task| !task.is_finished());
        if !self.shutting_down
            && let Some((turn_id, submission)) = self.pending.pop_front()
        {
            self.start_turn(turn_id, submission).await;
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
        while let Ok(Command::Submit { reply, .. }) = commands.try_recv() {
            let _ = reply.send(Err(SubmitError::ShuttingDown));
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
            for task in &self.turn_tasks {
                task.abort();
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
        if let Some(mut lifecycle) = self.turn.take()
            && let Ok(from) = lifecycle.transition(TurnState::Interrupted)
        {
            let cause = if aborted {
                "shutdown: task aborted"
            } else {
                "shutdown"
            };
            self.record_terminal(
                lifecycle.turn_id(),
                from,
                TurnState::Interrupted,
                Some(cause.into()),
            )
            .await;
        }

        self.pending.clear();
        self.publish_status();
        if let Err(err) = self.store.close().await {
            tracing::warn!(thread_id = %self.thread_id, error = %err, "thread store close failed");
        }
    }
}
