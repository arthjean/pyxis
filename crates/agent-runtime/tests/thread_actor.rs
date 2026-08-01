//! US-004: the thread actor owns admission, ordering, turn lifecycle and
//! shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use agent_core::model::{
    InputModality, ModelRetryPolicy, ModelRuntimeSource, ModelToolMode, ResolvedModelRuntime,
    ResponsesDialect, TruncationMode, TruncationPolicy,
};
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
use agent_runtime::event::{ThreadEvent, ThreadEventPayload};
use agent_runtime::id::{RandomIds, ThreadId, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::{TurnOutcome, TurnRequest, TurnRunner};
use agent_runtime::store::{
    FailingThreadStore, FailurePoint, MemoryThreadStore, StoreError, StoreOperation,
    ThreadSnapshot, ThreadStore,
};
use agent_runtime::thread::{
    COMMAND_MAILBOX, MAX_PENDING_INPUTS, RuntimeEventPayload, Submission, SubmitError,
    ThreadHandle, ThreadHealth, ThreadOptions,
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
    async fn commit_recovery(
        &self,
        repair: &agent_runtime::store::RecoveryCommit,
    ) -> Result<(), StoreError> {
        let _gate = self.gate.lock().await;
        self.inner.commit_recovery(repair).await
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

async fn start(
    store: Arc<dyn ThreadStore>,
    runner: Arc<dyn TurnRunner>,
) -> (Arc<ThreadHandle>, ThreadId, CancellationToken) {
    let contexts = Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
        &RandomIds,
    )))) as Arc<dyn TurnContextSource>;
    start_with_contexts(store, runner, contexts).await
}

async fn start_with_contexts(
    store: Arc<dyn ThreadStore>,
    runner: Arc<dyn TurnRunner>,
    turn_contexts: Arc<dyn TurnContextSource>,
) -> (Arc<ThreadHandle>, ThreadId, CancellationToken) {
    let thread_id = ThreadId::generate(&RandomIds);
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store,
        runner,
        turn_contexts,
        ids: Arc::new(RandomIds),
        clock: Arc::new(FixedClock),
        parent_cancel: root.clone(),
        agents: None,
    })
    .await
    .expect("the thread starts");
    (Arc::new(handle), thread_id, root)
}

fn resolved_runtime() -> ResolvedModelRuntime {
    ResolvedModelRuntime {
        slug: "test-model".into(),
        source: ModelRuntimeSource::Embedded {
            version: "test".into(),
        },
        instructions: "runtime instructions".into(),
        fingerprint: "a".repeat(64),
        context_window: 100_000,
        auto_compact_token_limit: 80_000,
        input_modalities: vec![InputModality::Text],
        reasoning_effort: Some("high".into()),
        supports_verbosity: true,
        verbosity: Some("low".into()),
        supports_parallel_tool_calls: true,
        service_tiers: vec!["priority".into()],
        reasoning_replay: agent_core::model::ReasoningReplaySupport::Enabled,
        responses_dialect: ResponsesDialect::Standard,
        tool_mode: ModelToolMode::Direct,
        multi_agent_version: Default::default(),
        truncation: TruncationPolicy {
            mode: TruncationMode::Tokens,
            limit: 2_000,
        },
        retry: ModelRetryPolicy {
            max_attempts: 4,
            backoff_base_ms: 50,
        },
        max_output_tokens: 4096,
        comp_hash: Some("test-hash".into()),
    }
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

fn store_event(thread_id: ThreadId) -> ThreadEvent {
    ThreadEvent {
        event_id: agent_runtime::EventId::generate(&RandomIds),
        thread_id,
        seq: 1,
        at_ms: 1,
        payload: ThreadEventPayload::ThreadCreated,
    }
}

// ───────── tests ─────────

#[tokio::test]
async fn the_failing_store_targets_named_ordinals_and_poisoned_writes_stay_inspectable() {
    for operation in [
        StoreOperation::Create,
        StoreOperation::Flush,
        StoreOperation::Read,
        StoreOperation::Close,
    ] {
        let inner = Arc::new(MemoryThreadStore::new());
        let failing = FailingThreadStore::new(
            Arc::clone(&inner) as Arc<dyn ThreadStore>,
            FailurePoint::before(operation, 1, format!("{operation} fault")),
        );
        let thread_id = ThreadId::generate(&RandomIds);
        let result = match operation {
            StoreOperation::Create => failing.create(&thread_id).await,
            StoreOperation::Flush => failing.flush().await,
            StoreOperation::Read => failing.read().await.map(|_| ()),
            StoreOperation::Close => failing.close().await,
            StoreOperation::Append | StoreOperation::CommitRecovery | StoreOperation::Fork => {
                continue;
            }
        };
        assert!(matches!(
            result,
            Err(StoreError::Injected {
                operation: failed,
                ref detail,
            }) if failed == operation && detail.contains(operation.to_string().as_str())
        ));
        assert_eq!(
            failing.calls().unwrap(),
            [agent_runtime::StoreCall {
                operation,
                ordinal: 1
            }]
        );
    }

    let inner = Arc::new(MemoryThreadStore::new());
    let thread_id = ThreadId::generate(&RandomIds);
    inner.create(&thread_id).await.unwrap();
    let failing = FailingThreadStore::new(
        Arc::clone(&inner) as Arc<dyn ThreadStore>,
        FailurePoint::after_touch(StoreOperation::Append, 1, "write reached the store"),
    );
    let event = store_event(thread_id);
    assert!(matches!(
        failing.append(&event).await,
        Err(StoreError::Injected {
            operation: StoreOperation::Append,
            ..
        })
    ));
    assert_eq!(
        inner.read().await.unwrap().events,
        std::slice::from_ref(&event)
    );
    assert!(matches!(
        failing.append(&event).await,
        Err(StoreError::Poisoned)
    ));
    assert_eq!(
        failing.calls().unwrap(),
        [
            agent_runtime::StoreCall {
                operation: StoreOperation::Append,
                ordinal: 1
            },
            agent_runtime::StoreCall {
                operation: StoreOperation::Append,
                ordinal: 2
            }
        ]
    );
}

#[tokio::test]
async fn a_running_commit_failure_keeps_the_accepted_input_queued_and_closes_admission() {
    let inner = Arc::new(MemoryThreadStore::new());
    let failing = Arc::new(FailingThreadStore::new(
        Arc::clone(&inner) as Arc<dyn ThreadStore>,
        // ThreadCreated, InputSubmitted, then Running.
        FailurePoint::before(StoreOperation::Append, 3, "running commit refused"),
    ));
    let (runner, started) = ScriptedRunner::new(Behavior::CompleteNow);
    let (handle, _, _root) = start(failing as Arc<dyn ThreadStore>, runner).await;
    let mut events = handle.subscribe();
    let submission = Submission {
        text: "durable queued input".into(),
        client_message_id: Some("client-queued".into()),
    };

    let accepted = handle.submit(submission.clone()).await.unwrap();
    wait_for(
        || matches!(handle.status().health, ThreadHealth::StoreFailed { .. }),
        "store failure health",
    )
    .await;
    let status = handle.status();
    assert!(matches!(
        status.health,
        ThreadHealth::StoreFailed {
            operation: StoreOperation::Append,
            ..
        }
    ));
    assert_eq!(status.pending_inputs, 1);
    assert!(started.lock().unwrap().is_empty(), "no engine was minted");
    assert_eq!(
        handle.submit(submission).await.unwrap(),
        accepted,
        "an idempotent replay still recovers the accepted identity"
    );
    assert!(matches!(
        handle.submit(Submission::new("new")).await,
        Err(SubmitError::StoreFailed)
    ));
    assert!(matches!(
        handle.steer(Submission::new("steer"), None).await,
        Err(SubmitError::StoreFailed)
    ));
    assert!(matches!(
        handle.fork(None).await,
        Err(agent_runtime::thread::ForkError::Submit(
            SubmitError::StoreFailed
        ))
    ));
    assert_eq!(handle.interrupt(None).await.unwrap(), None);

    let mut failures = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event.payload, RuntimeEventPayload::StoreFailed { .. }) {
            failures += 1;
        }
    }
    assert_eq!(failures, 1, "the live store failure is published once");
    let snapshot = inner.read().await.unwrap();
    assert!(snapshot.events.iter().any(|event| {
        event.event_id == accepted.event_id
            && matches!(&event.payload, ThreadEventPayload::InputSubmitted { .. })
    }));
    assert!(!states(&snapshot).contains(&TurnState::Running));
    handle.shutdown().await;
}

#[tokio::test]
async fn a_terminal_commit_failure_publishes_no_terminal_and_releases_no_slot() {
    let inner = Arc::new(MemoryThreadStore::new());
    let failing = Arc::new(FailingThreadStore::new(
        Arc::clone(&inner) as Arc<dyn ThreadStore>,
        // ThreadCreated, InputSubmitted, Running, then the terminal.
        FailurePoint::before(StoreOperation::Append, 4, "terminal commit refused"),
    ));
    let (runner, started) = ScriptedRunner::new(Behavior::CompleteNow);
    let (handle, _, _root) = start(failing as Arc<dyn ThreadStore>, runner).await;
    let mut events = handle.subscribe();

    handle.submit(Submission::new("finish")).await.unwrap();
    wait_for(
        || matches!(handle.status().health, ThreadHealth::StoreFailed { .. }),
        "terminal store failure",
    )
    .await;
    assert_eq!(started.lock().unwrap().len(), 1);
    assert_eq!(
        handle.status().turn.map(|turn| turn.state),
        Some(TurnState::Running),
        "the uncommitted terminal is not projected through status"
    );
    assert!(matches!(
        handle.submit(Submission::new("must not start")).await,
        Err(SubmitError::StoreFailed)
    ));
    let snapshot = inner.read().await.unwrap();
    assert_eq!(states(&snapshot), [TurnState::Running]);

    let mut terminals = 0;
    let mut failures = 0;
    while let Ok(event) = events.try_recv() {
        match event.payload {
            RuntimeEventPayload::TurnStateChanged { to, .. } if to.is_terminal() => terminals += 1,
            RuntimeEventPayload::StoreFailed { .. } => failures += 1,
            _ => {}
        }
    }
    assert_eq!(terminals, 0);
    assert_eq!(failures, 1);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_runtime_descriptor_is_persisted_once_and_turns_reference_it() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, started) = ScriptedRunner::new(Behavior::CompleteNow);
    let runtime = resolved_runtime();
    let contexts = Arc::new(FixedTurnContext::with_model_runtime(
        turn_context(TurnId::generate(&RandomIds)),
        runtime.clone(),
    )) as Arc<dyn TurnContextSource>;
    let (handle, _, _root) =
        start_with_contexts(Arc::clone(&store) as Arc<dyn ThreadStore>, runner, contexts).await;

    handle.submit(Submission::new("one")).await.unwrap();
    wait_for(
        || {
            started.lock().unwrap().len() == 1
                && handle
                    .status()
                    .turn
                    .is_some_and(|turn| turn.state == TurnState::Completed)
        },
        "first terminal",
    )
    .await;
    handle.submit(Submission::new("two")).await.unwrap();
    wait_for(
        || {
            started.lock().unwrap().len() == 2
                && handle
                    .status()
                    .turn
                    .is_some_and(|turn| turn.state == TurnState::Completed)
        },
        "second terminal",
    )
    .await;

    let snapshot = store.read().await.unwrap();
    let descriptors: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::ModelRuntimeResolved {
                fingerprint,
                runtime,
            } => Some((fingerprint, runtime)),
            _ => None,
        })
        .collect();
    assert_eq!(descriptors, [(&runtime.fingerprint, &runtime)]);
    let contexts: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::TurnStateChanged {
                context: Some(context),
                ..
            } => Some(context),
            _ => None,
        })
        .collect();
    assert_eq!(contexts.len(), 2);
    assert!(contexts.iter().all(|context| {
        context.model_runtime_fingerprint.as_deref() == Some(runtime.fingerprint.as_str())
    }));
    {
        let requests = started.lock().unwrap();
        assert!(requests.iter().all(|request| {
            request
                .model_runtime
                .as_ref()
                .map(|runtime| runtime.fingerprint.as_str())
                == Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        }));
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn an_invalid_runtime_fails_before_the_turn_runner_starts() {
    let store = Arc::new(MemoryThreadStore::new());
    let (runner, started) = ScriptedRunner::new(Behavior::CompleteNow);
    let mut runtime = resolved_runtime();
    runtime.instructions = "x".repeat(agent_core::model::MAX_MODEL_INSTRUCTIONS_BYTES + 1);
    let contexts = Arc::new(FixedTurnContext::with_model_runtime(
        turn_context(TurnId::generate(&RandomIds)),
        runtime,
    )) as Arc<dyn TurnContextSource>;
    let (handle, _, _root) =
        start_with_contexts(Arc::clone(&store) as Arc<dyn ThreadStore>, runner, contexts).await;

    handle.submit(Submission::new("must fail")).await.unwrap();
    wait_for(
        || {
            handle
                .status()
                .turn
                .is_some_and(|turn| turn.state == TurnState::Failed)
        },
        "runtime refusal",
    )
    .await;

    assert!(started.lock().unwrap().is_empty());
    let snapshot = store.read().await.unwrap();
    assert_eq!(states(&snapshot), [TurnState::Failed]);
    assert!(snapshot.events.iter().all(|event| !matches!(
        &event.payload,
        ThreadEventPayload::ModelRuntimeResolved { .. }
    )));
    let cause = snapshot
        .events
        .iter()
        .find_map(|event| match &event.payload {
            ThreadEventPayload::TurnStateChanged {
                cause: Some(cause), ..
            } => Some(cause),
            _ => None,
        });
    assert!(cause.is_some_and(|cause| {
        cause.contains("instructions exceed") && cause.chars().count() <= 500
    }));
    handle.shutdown().await;
}

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
