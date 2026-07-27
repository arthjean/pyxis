//! US-004: the thread actor owns admission, ordering, turn lifecycle and
//! shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnLimits};
use agent_runtime::event::{ThreadEvent, ThreadEventPayload};
use agent_runtime::id::{RandomIds, ThreadId, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::{TurnOutcome, TurnRequest, TurnRunner};
use agent_runtime::store::{MemoryThreadStore, StoreError, ThreadSnapshot, ThreadStore};
use agent_runtime::thread::{
    COMMAND_MAILBOX, MAX_PENDING_INPUTS, RuntimeEventPayload, Submission, SubmitError,
    ThreadHandle, ThreadOptions,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ───────── fakes ─────────

struct FixedClock;

#[async_trait::async_trait]
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

enum Behavior {
    /// Ends as soon as it starts.
    CompleteNow,
    /// Waits for the test to hand out a permit.
    WaitForPermit(Arc<tokio::sync::Semaphore>),
    /// Cooperative: ends when its token is cancelled.
    WaitForCancel,
    /// Non-cooperative: ignores the token entirely.
    IgnoreCancel,
}

struct ScriptedRunner {
    behavior: Behavior,
    started: Arc<Mutex<Vec<TurnRequest>>>,
}

impl ScriptedRunner {
    fn new(behavior: Behavior) -> (Arc<Self>, Arc<Mutex<Vec<TurnRequest>>>) {
        let started = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                behavior,
                started: Arc::clone(&started),
            }),
            started,
        )
    }
}

#[async_trait::async_trait]
impl TurnRunner for ScriptedRunner {
    async fn run_turn(
        &self,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        self.started.lock().unwrap().push(request.clone());
        let _ = events
            .send(AgentEvent::Text(format!("écho: {}", request.text)))
            .await;
        match &self.behavior {
            Behavior::CompleteNow => TurnOutcome::Completed,
            Behavior::WaitForPermit(permits) => {
                tokio::select! {
                    permit = permits.acquire() => {
                        // `forget` so the permit is consumed: dropping it would
                        // hand it straight to the next turn.
                        if let Ok(permit) = permit {
                            permit.forget();
                        }
                        TurnOutcome::Completed
                    }
                    () = cancel.cancelled() => TurnOutcome::Interrupted,
                }
            }
            Behavior::WaitForCancel => {
                cancel.cancelled().await;
                TurnOutcome::Interrupted
            }
            Behavior::IgnoreCancel => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                TurnOutcome::Completed
            }
        }
    }
}

/// Store whose appends queue behind a gate the test holds.
struct GatedStore {
    inner: MemoryThreadStore,
    gate: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait::async_trait]
impl ThreadStore for GatedStore {
    async fn create(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        self.inner.create(thread_id).await
    }
    async fn append(&self, event: &ThreadEvent) -> Result<(), StoreError> {
        let _gate = self.gate.lock().await;
        self.inner.append(event).await
    }
    async fn flush(&self) -> Result<(), StoreError> {
        self.inner.flush().await
    }
    async fn read(&self) -> Result<ThreadSnapshot, StoreError> {
        self.inner.read().await
    }
    async fn fork(
        &self,
        at: &agent_runtime::store::ForkPoint,
    ) -> Result<Arc<dyn ThreadStore>, StoreError> {
        self.inner.fork(at).await
    }
    async fn close(&self) -> Result<(), StoreError> {
        self.inner.close().await
    }
}

// ───────── helpers ─────────

pub fn turn_context(turn_id: TurnId) -> TurnContext {
    TurnContext {
        turn_id,
        model: "test-model".into(),
        reasoning_effort: None,
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

async fn start(
    store: Arc<dyn ThreadStore>,
    runner: Arc<dyn TurnRunner>,
) -> (Arc<ThreadHandle>, ThreadId, CancellationToken) {
    let thread_id = ThreadId::generate(&RandomIds);
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store,
        runner,
        turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
            &RandomIds,
        )))),
        ids: Arc::new(RandomIds),
        clock: Arc::new(FixedClock),
        parent_cancel: root.clone(),
    })
    .await
    .expect("the thread starts");
    (Arc::new(handle), thread_id, root)
}

fn states(snapshot: &ThreadSnapshot) -> Vec<TurnState> {
    snapshot
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            ThreadEventPayload::TurnStateChanged { to, .. } => Some(*to),
            _ => None,
        })
        .collect()
}

async fn wait_for(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(check(), "timed out waiting for {what}");
}

// ───────── tests ─────────

/// EP-001 definition of done: create, submit, observe, close, rebuild.
#[tokio::test]
async fn a_thread_is_rebuilt_from_its_store_after_shutdown() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, _) = ScriptedRunner::new(Behavior::CompleteNow);
    let (handle, thread_id, _root) =
        start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;
    let mut events = handle.subscribe();

    let accepted = handle
        .submit(Submission::new("bonjour"))
        .await
        .expect("submission accepted");

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(event)) => {
                let done = matches!(
                    &event.payload,
                    RuntimeEventPayload::TurnStateChanged {
                        to: TurnState::Completed,
                        ..
                    }
                );
                seen.push(event);
                if done {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    // The client saw the input, the engine output and both transitions, all
    // correlated to the same thread and turn.
    assert!(seen.iter().all(|e| e.thread_id == thread_id));
    assert!(matches!(
        seen.first().map(|e| &e.payload),
        Some(RuntimeEventPayload::InputAccepted { .. })
    ));
    assert!(
        seen.iter()
            .any(|e| matches!(&e.payload, RuntimeEventPayload::Engine(AgentEvent::Text(_)))),
        "engine events reach the client through the runtime: {seen:?}"
    );
    assert!(seen.iter().any(|e| matches!(
        &e.payload,
        RuntimeEventPayload::TurnStateChanged {
            to: TurnState::Completed,
            ..
        }
    )));

    handle.shutdown().await;

    let snapshot = store
        .read()
        .await
        .expect("the store is readable when closed");
    assert_eq!(snapshot.thread_id, Some(thread_id));
    assert!(matches!(
        snapshot.events.first().map(|e| &e.payload),
        Some(ThreadEventPayload::ThreadCreated)
    ));
    assert!(snapshot.events.iter().any(|e| matches!(
        &e.payload,
        ThreadEventPayload::InputSubmitted { turn_id, text, .. }
            if *turn_id == accepted.turn_id && text == "bonjour"
    )));
    assert_eq!(
        states(&snapshot),
        vec![TurnState::Running, TurnState::Completed]
    );
    let seqs: Vec<u64> = snapshot.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}

/// AC4: the acknowledgement never precedes durability.
#[tokio::test]
async fn an_acknowledged_submission_is_already_durable() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, _) = ScriptedRunner::new(Behavior::WaitForPermit(Arc::new(
        tokio::sync::Semaphore::new(0),
    )));
    let (handle, _, _root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    let accepted = handle.submit(Submission::new("durable")).await.unwrap();

    let snapshot = store.read().await.unwrap();
    assert!(
        snapshot
            .events
            .iter()
            .any(|e| e.event_id == accepted.event_id),
        "the submission event was durable before the acknowledgement came back"
    );
    handle.shutdown().await;
}

/// AC2: concurrent producers are serialized, without loss or duplicate.
#[tokio::test]
async fn concurrent_producers_are_serialized_by_the_mailbox() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, _) = ScriptedRunner::new(Behavior::CompleteNow);
    let (handle, _, _root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    let mut producers = Vec::new();
    for i in 0..MAX_PENDING_INPUTS {
        let handle = Arc::clone(&handle);
        producers.push(tokio::spawn(async move {
            handle.submit(Submission::new(format!("input-{i}"))).await
        }));
    }
    let mut turn_ids = Vec::new();
    for producer in producers {
        turn_ids.push(producer.await.unwrap().expect("accepted"));
    }

    let snapshot = store.read().await.unwrap();
    let inputs: Vec<&ThreadEvent> = snapshot
        .events
        .iter()
        .filter(|e| matches!(e.payload, ThreadEventPayload::InputSubmitted { .. }))
        .collect();
    assert_eq!(inputs.len(), MAX_PENDING_INPUTS, "no input lost or doubled");

    let mut seqs: Vec<u64> = inputs.iter().map(|e| e.seq).collect();
    let sorted = {
        let mut s = seqs.clone();
        s.sort_unstable();
        s.dedup();
        s
    };
    seqs.dedup();
    assert_eq!(seqs, sorted, "acceptance order is a total order in the log");

    let mut ids: Vec<String> = turn_ids.iter().map(|a| a.turn_id.to_string()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        MAX_PENDING_INPUTS,
        "every producer got its own turn"
    );
    handle.shutdown().await;
}

/// AC3: a single regular turn at a time.
#[tokio::test]
async fn a_second_regular_turn_waits_for_the_terminal_of_the_first() {
    let permits = Arc::new(tokio::sync::Semaphore::new(0));
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, started) = ScriptedRunner::new(Behavior::WaitForPermit(Arc::clone(&permits)));
    let (handle, _, _root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    let first = handle.submit(Submission::new("premier")).await.unwrap();
    wait_for(
        || started.lock().unwrap().len() == 1,
        "the first turn to start",
    )
    .await;

    let second = handle.submit(Submission::new("second")).await.unwrap();
    assert_ne!(first.turn_id, second.turn_id);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        started.lock().unwrap().len(),
        1,
        "the second turn must not start before the first reaches its terminal"
    );
    assert_eq!(handle.status().pending_inputs, 1);

    // One permit only: the first turn ends, the second takes its place and stays
    // running, which is what makes the hand-off observable.
    permits.add_permits(1);
    wait_for(
        || started.lock().unwrap().len() == 2,
        "the second turn to start",
    )
    .await;
    wait_for(
        || {
            handle
                .status()
                .turn
                .is_some_and(|t| t.turn_id == second.turn_id && t.state == TurnState::Running)
        },
        "the second turn to become active",
    )
    .await;
    assert_eq!(handle.status().pending_inputs, 0);

    permits.add_permits(1);
    handle.shutdown().await;
    let snapshot = store.read().await.unwrap();
    // The first turn is terminal BEFORE the second becomes running.
    assert_eq!(
        states(&snapshot)[..3],
        [TurnState::Running, TurnState::Completed, TurnState::Running]
    );
}

/// AC5: a saturated mailbox refuses immediately and announces no identifier.
#[tokio::test]
async fn a_full_mailbox_refuses_without_announcing_an_identifier() {
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let store = Arc::new(GatedStore {
        inner: MemoryThreadStore::new(),
        gate: Arc::clone(&gate),
    });
    let (runner, _) = ScriptedRunner::new(Behavior::CompleteNow);
    let (handle, _, _root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    // Block every append: the actor stalls on the first command, the mailbox
    // fills behind it.
    let held = Arc::clone(&gate).lock_owned().await;

    let refused = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let worst_latency = Arc::new(Mutex::new(Duration::ZERO));
    let mut producers = Vec::new();
    for i in 0..(COMMAND_MAILBOX + 8) {
        let handle = Arc::clone(&handle);
        let refused = Arc::clone(&refused);
        let accepted = Arc::clone(&accepted);
        let worst_latency = Arc::clone(&worst_latency);
        producers.push(tokio::spawn(async move {
            let started = Instant::now();
            match handle
                .submit(Submission::new(format!("saturation-{i}")))
                .await
            {
                Err(SubmitError::QueueFull) => {
                    let elapsed = started.elapsed();
                    let mut worst = worst_latency.lock().unwrap();
                    *worst = (*worst).max(elapsed);
                    refused.fetch_add(1, Ordering::SeqCst);
                }
                Ok(_) => {
                    accepted.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => {}
            }
        }));
    }

    wait_for(|| refused.load(Ordering::SeqCst) > 0, "a QueueFull refusal").await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "no identifier is announced while nothing can be persisted"
    );
    assert!(
        *worst_latency.lock().unwrap() < Duration::from_millis(100),
        "a refusal must come back in under 100 ms, took {:?}",
        worst_latency.lock().unwrap()
    );
    let snapshot = store.read().await.unwrap();
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| !matches!(e.payload, ThreadEventPayload::InputSubmitted { .. })),
        "a refused submission persists nothing"
    );

    drop(held);
    handle.shutdown().await;
    for producer in producers {
        let _ = tokio::time::timeout(Duration::from_secs(2), producer).await;
    }
}

/// AC6: shutdown closes admission, cancels, waits, then closes the store.
#[tokio::test]
async fn shutdown_cancels_tracked_tasks_then_closes_the_store() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, started) = ScriptedRunner::new(Behavior::WaitForCancel);
    let (handle, _, root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    handle
        .submit(Submission::new("longue tâche"))
        .await
        .unwrap();
    wait_for(|| started.lock().unwrap().len() == 1, "the turn to start").await;

    let elapsed = {
        let at = Instant::now();
        handle.shutdown().await;
        at.elapsed()
    };
    assert!(
        elapsed < Duration::from_secs(2),
        "a cooperative task reaches its terminal well inside the budget, took {elapsed:?}"
    );
    assert!(
        !root.is_cancelled(),
        "shutting a thread down must not cancel its parent domain"
    );

    let snapshot = store.read().await.unwrap();
    assert_eq!(
        states(&snapshot),
        vec![TurnState::Running, TurnState::Interrupted]
    );
    assert!(matches!(
        handle.submit(Submission::new("après")).await,
        Err(SubmitError::Stopped | SubmitError::ShuttingDown)
    ));
    assert!(matches!(
        store.append(&snapshot.events[0]).await,
        Err(StoreError::Closed)
    ));
}

/// AC6: a task that ignores cancellation is aborted, drained and still owes
/// exactly one terminal.
#[tokio::test]
async fn a_straggler_is_aborted_and_still_produces_one_terminal() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, started) = ScriptedRunner::new(Behavior::IgnoreCancel);
    let (handle, _, _root) = start(Arc::clone(&store) as Arc<dyn ThreadStore>, runner).await;

    handle
        .submit(Submission::new("non coopératif"))
        .await
        .unwrap();
    wait_for(|| started.lock().unwrap().len() == 1, "the turn to start").await;

    let at = Instant::now();
    handle.shutdown().await;
    let elapsed = at.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2),
        "the straggler got its grace period, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the full shutdown stays bounded, took {elapsed:?}"
    );

    let snapshot = store.read().await.unwrap();
    let terminals: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            ThreadEventPayload::TurnStateChanged { to, cause, .. } if to.is_terminal() => {
                Some((*to, cause.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal: {terminals:?}");
    assert_eq!(terminals[0].0, TurnState::Interrupted);
    assert_eq!(
        terminals[0].1.as_deref(),
        Some("shutdown: task aborted"),
        "the forced stop is named in the log"
    );
}
