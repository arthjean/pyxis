//! US-009: resuming a thread and deduplicating submissions.
//!
//! The store here survives its own `close`, which is what a file does and what a
//! `MemoryThreadStore` deliberately does not: reopening one is exactly what a
//! restart does, so these tests exercise the runtime's resume path and not an
//! adapter's.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::message::{ContentBlock, Message};
use agent_runtime::context::{FixedTurnContext, TurnContextSource};
use agent_runtime::event::{ThreadEvent, ThreadEventPayload};
use agent_runtime::id::{EventId, RandomIds, ThreadId, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::{RunAgentRunner, TurnRunner};
use agent_runtime::store::{
    FailingThreadStore, FailurePoint, ForkPoint, RecoveryCommit, StoreError, StoreOperation,
    ThreadSnapshot, ThreadStore,
};
use agent_runtime::thread::{Submission, ThreadHandle, ThreadOptions};
use common::{FakeProvider, FakeSession, Scripted, done_end_turn, text, wait_for_terminal};
use tokio_util::sync::CancellationToken;

// ───────── a durable log that outlives its store ─────────

#[derive(Clone, Default)]
struct Log {
    thread_id: Arc<Mutex<Option<ThreadId>>>,
    events: Arc<Mutex<Vec<ThreadEvent>>>,
    messages: Arc<Mutex<Vec<Message>>>,
    repairs: Arc<Mutex<Vec<RecoveryCommit>>>,
}

impl Log {
    fn events(&self) -> Vec<ThreadEvent> {
        self.events.lock().unwrap().clone()
    }

    fn repairs(&self) -> Vec<EventId> {
        self.repairs
            .lock()
            .unwrap()
            .iter()
            .map(|repair| repair.repair_id)
            .collect()
    }

    /// Appends without going through a store: how a previous process left the
    /// file behind.
    fn seed(&self, thread_id: ThreadId, payloads: Vec<ThreadEventPayload>) {
        *self.thread_id.lock().unwrap() = Some(thread_id);
        let ids = RandomIds;
        let mut events = self.events.lock().unwrap();
        for payload in payloads {
            let seq = events.len() as u64 + 1;
            events.push(ThreadEvent {
                event_id: EventId::generate(&ids),
                thread_id,
                seq,
                at_ms: seq,
                payload,
            });
        }
    }
}

struct SharedStore {
    log: Log,
    closed: AtomicBool,
}

impl SharedStore {
    fn open(log: &Log) -> Arc<Self> {
        Arc::new(Self {
            log: log.clone(),
            closed: AtomicBool::new(false),
        })
    }

    fn live(&self) -> Result<(), StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StoreError::Closed);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ThreadStore for SharedStore {
    async fn create(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        self.live()?;
        let mut bound = self.log.thread_id.lock().unwrap();
        match *bound {
            Some(held) if held != *thread_id => Err(StoreError::ThreadMismatch {
                holds: held,
                given: *thread_id,
            }),
            Some(_) => Ok(()),
            None => {
                *bound = Some(*thread_id);
                Ok(())
            }
        }
    }
    async fn append(&self, event: &ThreadEvent) -> Result<(), StoreError> {
        self.live()?;
        self.log.events.lock().unwrap().push(event.clone());
        Ok(())
    }
    async fn commit_recovery(&self, repair: &RecoveryCommit) -> Result<(), StoreError> {
        self.live()?;
        let mut repairs = self.log.repairs.lock().unwrap();
        if let Some(applied) = repairs
            .iter()
            .find(|applied| applied.repair_id == repair.repair_id)
        {
            return if applied == repair {
                Ok(())
            } else {
                Err(StoreError::InvalidRecovery(format!(
                    "repair ID {} was reused with another body",
                    repair.repair_id
                )))
            };
        }
        let thread_id = self
            .log
            .thread_id
            .lock()
            .unwrap()
            .ok_or_else(|| StoreError::InvalidRecovery("shared log is unbound".into()))?;
        let mut events = self.log.events.lock().unwrap();
        let next_seq = events.last().map_or(1, |event| event.seq.saturating_add(1));
        repair.validate(thread_id, next_seq)?;
        self.log
            .messages
            .lock()
            .unwrap()
            .extend(repair.tool_results.clone());
        events.extend(repair.closures.clone());
        repairs.push(repair.clone());
        Ok(())
    }
    async fn flush(&self) -> Result<(), StoreError> {
        self.live()
    }
    async fn read(&self) -> Result<ThreadSnapshot, StoreError> {
        Ok(ThreadSnapshot {
            thread_id: *self.log.thread_id.lock().unwrap(),
            messages: self.log.messages.lock().unwrap().clone(),
            events: self.log.events(),
            ..ThreadSnapshot::default()
        })
    }
    async fn fork(&self, at: &ForkPoint) -> Result<Arc<dyn ThreadStore>, StoreError> {
        let events = self.log.events();
        let cut = events
            .iter()
            .position(|e| e.event_id == at.fork_event_id)
            .ok_or(StoreError::NoSuchEvent {
                event_id: at.fork_event_id,
            })?;
        let branch = Log {
            thread_id: Arc::new(Mutex::new(Some(at.child_thread_id))),
            events: Arc::new(Mutex::new(events[..=cut].to_vec())),
            messages: Arc::new(Mutex::new(self.log.messages.lock().unwrap().clone())),
            repairs: Arc::new(Mutex::new(Vec::new())),
        };
        Ok(SharedStore::open(&branch) as Arc<dyn ThreadStore>)
    }
    async fn close(&self) -> Result<(), StoreError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// ───────── wiring ─────────

async fn open(log: &Log, thread_id: ThreadId, runner: Arc<dyn TurnRunner>) -> ThreadHandle {
    ThreadHandle::start(options(
        thread_id,
        SharedStore::open(log) as Arc<dyn ThreadStore>,
        runner,
    ))
    .await
    .expect("the thread opens")
}

fn options(
    thread_id: ThreadId,
    store: Arc<dyn ThreadStore>,
    runner: Arc<dyn TurnRunner>,
) -> ThreadOptions {
    ThreadOptions {
        thread_id,
        store,
        runner,
        turn_contexts: Arc::new(FixedTurnContext::new(common::turn_context(
            TurnId::generate(&RandomIds),
        ))) as Arc<dyn TurnContextSource>,
        ids: Arc::new(RandomIds),
        clock: Arc::new(common::InstantClock),
        parent_cancel: CancellationToken::new(),
        agents: None,
    }
}

fn echo_runner(turns: usize) -> Arc<dyn TurnRunner> {
    let scripted = (0..turns.max(1))
        .map(|_| Scripted::Stream(vec![text("ok"), done_end_turn()]))
        .collect();
    let deps = common::deps(
        FakeProvider::new(scripted),
        FakeSession::new(),
        Arc::new(common::EchoTools),
    );
    Arc::new(RunAgentRunner::new(deps, common::agent_context))
}

// ───────── AC1: a clean close comes back exactly as it was ─────────

#[tokio::test]
async fn reopening_a_closed_thread_rebuilds_its_last_state_and_its_submissions() {
    let log = Log::default();
    let thread_id = ThreadId::generate(&RandomIds);

    let first = open(&log, thread_id, echo_runner(1)).await;
    let accepted = first
        .submit(Submission {
            text: "un".into(),
            client_message_id: Some("cli-1".into()),
        })
        .await
        .unwrap();
    let terminal = wait_for_terminal(&first).await;
    first.shutdown().await;
    let durable = log.events();
    // What the engine's own session persisted during that turn.
    let transcript = vec![Message::user("un"), Message::assistant_text("ok")];
    *log.messages.lock().unwrap() = transcript.clone();

    let second = open(&log, thread_id, echo_runner(1)).await;
    let resumed = second.resumed();
    assert_eq!(resumed.thread_id, thread_id);
    assert_eq!(resumed.messages, transcript, "the transcript comes back");
    assert_eq!(resumed.turn, Some(terminal), "the last turn comes back");
    assert_eq!(
        resumed.turn_context.as_ref().map(|c| c.turn_id),
        Some(accepted.turn_id),
        "the configuration captured for that turn comes back with it"
    );
    assert_eq!(
        resumed.turn_context.as_ref().map(|c| c.model.as_str()),
        Some("test-model")
    );
    assert!(resumed.recovered.is_empty(), "nothing to recover");
    assert_eq!(resumed.accepted["cli-1"], accepted);
    assert!(resumed.agents.is_empty());
    assert_eq!(second.status().turn, Some(terminal));
    assert_eq!(
        log.events(),
        durable,
        "resuming a clean thread appends nothing"
    );
    second.shutdown().await;
}

// ───────── AC2: a crash leaves a turn open; resume closes it once ─────────

#[tokio::test]
async fn a_turn_left_open_by_a_crash_is_recovered_once_after_reconciliation() {
    let log = Log::default();
    let thread_id = ThreadId::generate(&RandomIds);
    let turn_id = TurnId::generate(&RandomIds);
    log.seed(
        thread_id,
        vec![
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id: None,
                text: "lance l'outil".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
        ],
    );
    // The transcript a crash left behind: a tool call nobody answered.
    *log.messages.lock().unwrap() = vec![
        Message::user("lance l'outil"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]),
    ];

    let handle = open(&log, thread_id, echo_runner(1)).await;
    let resumed = handle.resumed();
    assert_eq!(resumed.recovered, vec![turn_id]);
    assert_eq!(resumed.reconciled_calls, 1);
    assert!(
        agent_core::message::unanswered_tool_calls(&resumed.messages).is_empty(),
        "the resumed transcript carries no orphan tool call"
    );
    assert_eq!(
        resumed.turn.map(|t| t.state),
        Some(TurnState::Interrupted),
        "the crashed turn is terminal again"
    );

    let recoveries: Vec<_> = log
        .events()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.payload,
                ThreadEventPayload::TurnStateChanged {
                    to: TurnState::Interrupted,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(recoveries.len(), 1, "exactly one recovery event");
    assert_eq!(log.repairs().len(), 1, "one physical recovery commit");
    assert!(
        matches!(
            &recoveries[0].payload,
            ThreadEventPayload::TurnStateChanged { cause: Some(cause), .. } if cause.contains("recovered")
        ),
        "the recovery names its cause: {:?}",
        recoveries[0].payload
    );
    handle.shutdown().await;

    // Reopening again must not write a second terminal for the same turn.
    let after = log.events().len();
    let again = open(&log, thread_id, echo_runner(1)).await;
    assert!(again.resumed().recovered.is_empty());
    assert_eq!(log.events().len(), after, "recovery is not repeated");
    assert_eq!(
        log.repairs().len(),
        1,
        "the physical repair is not repeated"
    );
    again.shutdown().await;
}

#[tokio::test]
async fn a_queued_input_is_promoted_once_instead_of_being_closed_by_resume() {
    let log = Log::default();
    let thread_id = ThreadId::generate(&RandomIds);
    let turn_id = TurnId::generate(&RandomIds);
    log.seed(
        thread_id,
        vec![
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id: Some("queued-1".into()),
                text: "reprends".into(),
            },
        ],
    );

    let handle = open(&log, thread_id, echo_runner(1)).await;
    let terminal = wait_for_terminal(&handle).await;
    assert_eq!(terminal.turn_id, turn_id);
    assert_eq!(terminal.state, TurnState::Completed);
    assert!(handle.resumed().recovered.is_empty());
    let states: Vec<_> = log
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::TurnStateChanged {
                turn_id: id, to, ..
            } if *id == turn_id => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(states, [TurnState::Running, TurnState::Completed]);
    handle.shutdown().await;

    let durable_len = log.events().len();
    let reopened = open(&log, thread_id, echo_runner(1)).await;
    assert_eq!(reopened.status().turn, Some(terminal));
    assert_eq!(
        log.events().len(),
        durable_len,
        "a terminal queued turn is not started twice"
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn recovery_is_all_or_nothing_across_failures_before_and_after_the_commit() {
    for after_touch in [false, true] {
        let log = Log::default();
        let thread_id = ThreadId::generate(&RandomIds);
        let turn_id = TurnId::generate(&RandomIds);
        log.seed(
            thread_id,
            vec![
                ThreadEventPayload::ThreadCreated,
                ThreadEventPayload::InputSubmitted {
                    turn_id,
                    client_message_id: None,
                    text: "outil".into(),
                },
                ThreadEventPayload::TurnStateChanged {
                    turn_id,
                    from: Some(TurnState::Queued),
                    to: TurnState::Running,
                    cause: None,
                    context: None,
                },
            ],
        );
        *log.messages.lock().unwrap() = vec![Message::assistant(vec![ContentBlock::ToolUse {
            id: "orphan".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }])];

        let point = if after_touch {
            FailurePoint::after_touch(
                StoreOperation::CommitRecovery,
                1,
                "crash after recovery write",
            )
        } else {
            FailurePoint::before(
                StoreOperation::CommitRecovery,
                1,
                "crash before recovery write",
            )
        };
        let failing = Arc::new(FailingThreadStore::new(
            SharedStore::open(&log) as Arc<dyn ThreadStore>,
            point,
        ));
        let failed = ThreadHandle::start(options(
            thread_id,
            failing as Arc<dyn ThreadStore>,
            echo_runner(1),
        ))
        .await;
        assert!(
            matches!(
                failed,
                Err(agent_runtime::thread::RuntimeError::StoreOperation {
                    operation: StoreOperation::CommitRecovery,
                    ..
                })
            ),
            "a failed repair publishes no commandable actor"
        );
        assert_eq!(log.repairs().len(), if after_touch { 1 } else { 0 });

        let reopened = open(&log, thread_id, echo_runner(1)).await;
        assert!(
            reopened
                .resumed()
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        );
        assert_eq!(log.repairs().len(), 1, "the repair lands exactly once");
        assert_eq!(
            log.events()
                .iter()
                .filter(|event| matches!(
                    &event.payload,
                    ThreadEventPayload::TurnStateChanged {
                        to: TurnState::Interrupted,
                        ..
                    }
                ))
                .count(),
            1
        );
        reopened.shutdown().await;
    }
}

// ───────── AC3: a replayed submission is not a second submission ─────────

#[tokio::test]
async fn a_replayed_client_message_id_returns_the_original_identifiers() {
    let log = Log::default();
    let thread_id = ThreadId::generate(&RandomIds);
    let handle = open(&log, thread_id, echo_runner(2)).await;

    let submission = Submission {
        text: "une seule fois".into(),
        client_message_id: Some("cli-42".into()),
    };
    let first = handle.submit(submission.clone()).await.unwrap();
    let replay = handle.submit(submission.clone()).await.unwrap();
    assert_eq!(
        replay, first,
        "the client gets its original identifiers back"
    );
    wait_for_terminal(&handle).await;

    let inputs = |events: Vec<ThreadEvent>| {
        events
            .into_iter()
            .filter(|e| matches!(e.payload, ThreadEventPayload::InputSubmitted { .. }))
            .count()
    };
    assert_eq!(inputs(log.events()), 1, "a replay appends no second input");
    handle.shutdown().await;

    // The dedup survives the restart: the map is rebuilt from the log.
    let after = log.events().len();
    let reopened = open(&log, thread_id, echo_runner(2)).await;
    assert_eq!(reopened.submit(submission).await.unwrap(), first);
    assert_eq!(
        log.events().len(),
        after,
        "a replay across a restart is still a no-op"
    );
    assert_eq!(inputs(log.events()), 1);
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_steer_carrying_a_replayed_identifier_is_not_queued_twice() {
    let provider = FakeProvider::new(vec![
        Scripted::StreamThenHang(vec![text("en cours")]),
        Scripted::Stream(vec![text("ok"), done_end_turn()]),
    ]);
    let deps = common::deps(provider, FakeSession::new(), Arc::new(common::EchoTools));
    let runner = Arc::new(RunAgentRunner::new(deps, common::agent_context));
    let log = Log::default();
    let thread_id = ThreadId::generate(&RandomIds);
    let handle = open(&log, thread_id, runner).await;

    handle.submit(Submission::new("un")).await.unwrap();
    common::wait_for(
        || {
            handle
                .status()
                .turn
                .is_some_and(|t| t.state == TurnState::Running)
        },
        "the turn to be running",
    )
    .await;

    let steer = Submission {
        text: "corrige".into(),
        client_message_id: Some("cli-steer".into()),
    };
    let first = handle.steer(steer.clone(), None).await.unwrap();
    let pending = handle.status().pending_steers;
    let replay = handle.steer(steer, None).await.unwrap();
    assert_eq!(replay, first);
    assert_eq!(
        handle.status().pending_steers,
        pending,
        "a replayed steer is not queued a second time"
    );

    handle.interrupt(None).await.unwrap();
    wait_for_terminal(&handle).await;
    handle.shutdown().await;
}
