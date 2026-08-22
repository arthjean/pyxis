//! The clock in the actor (EP-048).
//!
//! Everything here goes through [`ThreadHandle::start`]: the reminder is
//! created through the thread's own journal, the way the tool of EP-049 will
//! create it, the timer arm is the real fifth arm of the real `select!`, and
//! what is asserted is the DURABLE log, because that is what a reopened thread
//! reads.
//!
//! No test in this file waits for a real duration. Time is a [`TestClock`] with
//! two hands, one wall and one monotonic, and the only real waiting is the
//! bounded settling of [`settle`], capped well under the fifty milliseconds
//! US-155 allows.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
use agent_runtime::event::{ThreadEvent, ThreadEventPayload};
use agent_runtime::id::{ScheduleId, SequentialIds, ThreadId, TurnId};
use agent_runtime::jobs::{
    CompletionDelivery, JobKind, JobLaunch, JobLauncher, JobOutcome, JobProcess, JobRegistry,
    MAX_CONSECUTIVE_WAKES,
};
use agent_runtime::runner::{TurnOutcome, TurnRequest, TurnRunner};
use agent_runtime::schedule::{
    MAX_TIMER_SEGMENT, ScheduleRecord, ScheduleRule, dispatch_message_id,
};
use agent_runtime::store::{
    FailingThreadStore, FailurePoint, MemoryThreadStore, StoreOperation, ThreadStore,
};
use agent_runtime::thread::{MAX_PENDING_INPUTS, Submission, ThreadHandle, ThreadOptions};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const NOW: u64 = 1_770_000_000_000;
const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

// -- the clock ------------------------------------------------------------

/// A clock with two hands, because the two the runtime uses can disagree.
///
/// `wall` is what [`Clock::now_ms`] answers and what every scheduling decision
/// is made against. `mono` is what a sleep actually waits on, and it is what a
/// `CLOCK_MONOTONIC` sleep would wait on: it never goes backwards and it is not
/// affected by an NTP correction. Splitting them is what makes the two failures
/// of US-155 expressible at all, a wall clock stepped backwards under a sleep
/// that keeps running, and a wall clock that jumps forward while the monotonic
/// one crawls.
struct TestClock {
    wall: AtomicU64,
    mono: AtomicU64,
    /// Every segment the timer arm asked for, in order.
    sleeps: std::sync::Mutex<Vec<Duration>>,
}

impl TestClock {
    fn at(wall: u64) -> Arc<Self> {
        Arc::new(Self {
            wall: AtomicU64::new(wall),
            mono: AtomicU64::new(0),
            sleeps: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn now(&self) -> u64 {
        self.wall.load(Ordering::SeqCst)
    }

    /// Ordinary passage of time: both hands move together.
    fn advance(&self, ms: u64) {
        self.wall.fetch_add(ms, Ordering::SeqCst);
        self.mono.fetch_add(ms, Ordering::SeqCst);
    }

    /// The wall clock is corrected backwards while the monotonic one keeps
    /// going: an NTP step, or a hand-set clock.
    fn step_wall_back(&self, ms: u64) {
        self.wall.fetch_sub(ms, Ordering::SeqCst);
    }

    /// The wall clock jumps forward without the monotonic one following: the
    /// machine was suspended.
    fn suspend(&self, ms: u64) {
        self.wall.fetch_add(ms, Ordering::SeqCst);
    }

    fn segments(&self) -> Vec<Duration> {
        self.sleeps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now()
    }

    async fn sleep(&self, dur: Duration) {
        self.sleeps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(dur);
        let deadline = self
            .mono
            .load(Ordering::SeqCst)
            .saturating_add(dur.as_millis() as u64);
        while self.mono.load(Ordering::SeqCst) < deadline {
            tokio::task::yield_now().await;
        }
    }
}

// -- the harness ----------------------------------------------------------

/// A runner most tests never reach: a reminder is dispatched by the actor, not
/// by a model.
struct IdleRunner;

#[async_trait::async_trait]
impl TurnRunner for IdleRunner {
    async fn run_turn(
        &self,
        _request: TurnRequest,
        _events: mpsc::Sender<AgentEvent>,
        _cancel: CancellationToken,
    ) -> TurnOutcome {
        TurnOutcome::Completed
    }
}

/// A runner that holds its turn open until the test releases it. What makes a
/// reminder land DURING a turn observable.
struct HeldRunner {
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    started: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl TurnRunner for HeldRunner {
    async fn run_turn(
        &self,
        _request: TurnRequest,
        _events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        self.started.fetch_add(1, Ordering::SeqCst);
        let held = self.release.lock().await.take();
        if let Some(held) = held {
            tokio::select! {
                _ = held => {}
                () = cancel.cancelled() => return TurnOutcome::Interrupted,
            }
        }
        TurnOutcome::Completed
    }
}

struct TestLauncher;

#[async_trait::async_trait]
impl JobLauncher for TestLauncher {
    async fn launch(&self, _launch: JobLaunch) -> Result<Arc<dyn JobProcess>, String> {
        Ok(Arc::new(TestProcess) as Arc<dyn JobProcess>)
    }
}

struct TestProcess;

#[async_trait::async_trait]
impl JobProcess for TestProcess {
    async fn read_output(&self) -> Result<agent_runtime::jobs::JobOutput, String> {
        Ok(agent_runtime::jobs::JobOutput::default())
    }

    async fn stop(&self) {}
}

fn turn_context(turn_id: TurnId) -> TurnContext {
    TurnContext {
        turn_id,
        model: "gpt-5.4-codex".into(),
        reasoning_effort: None,
        model_runtime_fingerprint: None,
        permission_mode: "ask".into(),
        sandbox: "workspace-write".into(),
        workspace: std::path::PathBuf::from("/tmp"),
        limits: TurnLimits {
            max_output_tokens: 1024,
            max_pending_inputs: MAX_PENDING_INPUTS,
        },
    }
}

struct Opened {
    handle: ThreadHandle,
    jobs: Arc<JobRegistry>,
    root: CancellationToken,
}

struct Setup {
    store: Arc<dyn ThreadStore>,
    clock: Arc<TestClock>,
    seed: u64,
    delivery: CompletionDelivery,
    runner: Arc<dyn TurnRunner>,
}

impl Setup {
    fn new(store: Arc<dyn ThreadStore>, clock: Arc<TestClock>) -> Self {
        Self {
            store,
            clock,
            seed: 1,
            delivery: CompletionDelivery::Wake,
            runner: Arc::new(IdleRunner),
        }
    }

    fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    fn delivery(mut self, delivery: CompletionDelivery) -> Self {
        self.delivery = delivery;
        self
    }

    fn runner(mut self, runner: Arc<dyn TurnRunner>) -> Self {
        self.runner = runner;
        self
    }

    async fn open(self) -> Opened {
        let ids = Arc::new(SequentialIds::starting_at(self.seed));
        let thread_id = self
            .store
            .read()
            .await
            .expect("the store reads")
            .thread_id
            .unwrap_or_else(|| ThreadId::generate(ids.as_ref()));
        let jobs = JobRegistry::new(
            Arc::clone(&ids) as Arc<_>,
            Arc::clone(&self.clock) as Arc<dyn Clock>,
            Some(Arc::new(TestLauncher) as Arc<dyn JobLauncher>),
        );
        let root = CancellationToken::new();
        let handle = ThreadHandle::start(ThreadOptions {
            thread_id,
            store: Arc::clone(&self.store),
            runner: self.runner,
            turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
                ids.as_ref(),
            )))) as Arc<dyn TurnContextSource>,
            ids,
            clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            parent_cancel: root.clone(),
            agents: None,
            jobs: Some(Arc::clone(&jobs)),
            completion_delivery: self.delivery,
        })
        .await
        .expect("the thread starts");
        Opened { handle, jobs, root }
    }
}

/// Creates a reminder through the thread's own durable writer: the entry point
/// the scheduling tool will use, and the only one that exists today.
async fn schedule(handle: &ThreadHandle, id: ScheduleId, rule: ScheduleRule, now_ms: u64) {
    let record =
        ScheduleRecord::create(id, rule, "arroser les plantes", now_ms).expect("the rule is valid");
    handle
        .journal()
        .record(ThreadEventPayload::ScheduleCreated {
            schedule_id: record.schedule_id,
            rule: record.rule,
            prompt: record.prompt.clone(),
            due_at_ms: record.due_at_ms,
        })
        .await
        .expect("the creation is durable");
}

/// Lets the actor make progress until `want` holds, or gives up.
///
/// Yields first and sleeps only as a last resort: the whole point of a fake
/// clock is that nothing here waits for time to pass, and the budget below
/// stays an order of magnitude under the fifty milliseconds US-155 allows.
async fn settle(store: &Arc<dyn ThreadStore>, want: impl Fn(&[ThreadEvent]) -> bool) -> bool {
    for round in 0..600u32 {
        let events = store.read().await.expect("the store reads").events;
        if want(&events) {
            return true;
        }
        if round < 500 {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(Duration::from_micros(50)).await;
        }
    }
    false
}

/// Lets the actor run without expecting anything, so a NEGATIVE claim is made
/// against a thread that had every chance to act.
async fn quiesce() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

async fn events(store: &Arc<dyn ThreadStore>) -> Vec<ThreadEvent> {
    store.read().await.expect("the store reads").events
}

fn dispatches(events: &[ThreadEvent]) -> Vec<ScheduleId> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::ScheduleDispatched { schedule_id, .. } => Some(*schedule_id),
            _ => None,
        })
        .collect()
}

fn inputs(events: &[ThreadEvent]) -> Vec<(TurnId, Option<String>, String)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id,
                text,
            } => Some((*turn_id, client_message_id.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn has_dispatch(events: &[ThreadEvent]) -> bool {
    !dispatches(events).is_empty()
}

fn has_input(events: &[ThreadEvent]) -> bool {
    !inputs(events).is_empty()
}

// -- US-155: the bounded timer arm ---------------------------------------

/// AC5, AC9. A thread holding no reminder contributes a future that never
/// resolves: it asks for no sleep at all, so nothing can wake it on its own
/// account.
#[tokio::test]
async fn a_thread_without_a_reminder_never_arms_the_timer() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;

    clock.advance(DAY);
    quiesce().await;

    assert!(
        clock.segments().is_empty(),
        "a thread with nothing scheduled asked for a sleep"
    );
    assert!(!has_dispatch(&events(&store).await));
    opened.handle.shutdown().await;
}

/// AC6. A target three seconds out sleeps three seconds, and not a whole
/// segment: the bound is a ceiling, never a quantum.
#[tokio::test]
async fn a_target_three_seconds_out_sleeps_three_seconds_and_not_a_whole_segment() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 3 }, NOW).await;
    assert!(
        settle(&store, |_| !clock.segments().is_empty()).await,
        "the arm never asked for a sleep"
    );

    assert_eq!(
        clock.segments().first().copied(),
        Some(Duration::from_secs(3)),
        "the first segment was not the remaining time"
    );
    assert!(Duration::from_secs(3) < MAX_TIMER_SEGMENT);

    clock.advance(3 * SECOND);
    assert!(settle(&store, has_dispatch).await, "nothing was dispatched");
    opened.handle.shutdown().await;
}

/// AC4, AC7. A target three hours out is waited for in segments of at most a
/// minute, and the wall clock is read again after every one of them. The count
/// is what proves the arm never trusted a single long sleep.
#[tokio::test]
async fn a_target_three_hours_out_wakes_by_segments_and_rereads_the_clock() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::At {
            at_ms: NOW + 3 * HOUR,
        },
        NOW,
    )
    .await;

    // Three hours pass a minute at a time, which is exactly what a machine
    // running normally would do.
    for minute in 1..=180usize {
        assert!(
            settle(&store, |_| clock.segments().len() >= minute).await,
            "the arm did not rearm for minute {minute}"
        );
        clock.advance(MINUTE);
    }
    assert!(settle(&store, has_dispatch).await, "nothing was dispatched");

    let segments = clock.segments();
    assert_eq!(
        segments.len(),
        180,
        "three hours were not covered one segment at a time"
    );
    assert!(
        segments.iter().all(|segment| *segment <= MAX_TIMER_SEGMENT),
        "a segment was longer than the bound"
    );
    assert_eq!(MAX_TIMER_SEGMENT, Duration::from_secs(60));
    opened.handle.shutdown().await;
}

/// AC8. A wall clock stepped BACKWARDS after the arm was armed dispatches
/// nothing: the segment expires on the monotonic hand, the arm reads the wall
/// clock again, finds the target still ahead, and rearms. A wake never fires by
/// itself.
#[tokio::test]
async fn a_wall_clock_stepped_backwards_dispatches_nothing_and_rearms() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::At {
            at_ms: NOW + 30 * SECOND,
        },
        NOW,
    )
    .await;
    assert!(settle(&store, |_| !clock.segments().is_empty()).await);

    // The correction lands under the sleep: the monotonic hand covers the whole
    // segment, and the wall clock ends up further from the target than it
    // started.
    clock.step_wall_back(10 * MINUTE);
    clock.advance(30 * SECOND);
    quiesce().await;

    assert!(
        !has_dispatch(&events(&store).await),
        "a backwards clock produced a dispatch"
    );
    assert!(
        clock.segments().len() > 1,
        "the arm did not rearm after the corrected segment"
    );

    // And once the clock genuinely reaches the target, the reminder is still
    // there and is delivered.
    clock.advance(11 * MINUTE);
    assert!(
        settle(&store, has_dispatch).await,
        "the reminder was lost by the correction"
    );
    opened.handle.shutdown().await;
}

/// Edge case 20. A machine suspended for two hours wakes with a monotonic hand
/// that crawled and a wall clock that jumped. The segment is what notices, so
/// the reminder is late by at most one segment and never lost.
#[tokio::test]
async fn a_suspended_machine_delivers_within_one_segment_of_waking() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::At {
            at_ms: NOW + 3 * HOUR,
        },
        NOW,
    )
    .await;
    assert!(settle(&store, |_| !clock.segments().is_empty()).await);

    // The wall clock crosses the target while the machine was asleep.
    clock.suspend(3 * HOUR);
    // The current segment then expires on the monotonic hand alone.
    clock.advance(MINUTE);

    assert!(
        settle(&store, has_dispatch).await,
        "the reminder survived the suspend but was never delivered"
    );
    opened.handle.shutdown().await;
}

/// AC1. The fifth arm is LAST under `biased;`: a cancellation reaches the loop
/// with a reminder already due, and the loop breaks without dispatching it.
#[tokio::test]
async fn a_cancellation_wins_over_a_due_reminder() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .delivery(CompletionDelivery::Quiet)
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    // Due and cancelled in the same breath: the arm is ready to fire, and the
    // cancellation arm sits above it.
    clock.advance(30 * SECOND);
    opened.root.cancel();
    opened.handle.shutdown().await;

    assert!(
        !has_dispatch(&events(&store).await),
        "a cancelled thread still dispatched a reminder"
    );
}

// -- US-156: durable before delivered, exactly once ----------------------

/// AC1, AC2. The dispatch entry is durable BEFORE the input it produces, and
/// the input carries the key derived from the reminder and its slot.
#[tokio::test]
async fn the_dispatch_entry_is_durable_before_the_input_it_submits() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    clock.advance(30 * SECOND);
    assert!(settle(&store, has_input).await, "nothing was submitted");

    let log = events(&store).await;
    let dispatched = log
        .iter()
        .position(|event| matches!(event.payload, ThreadEventPayload::ScheduleDispatched { .. }))
        .expect("the dispatch is durable");
    let submitted = log
        .iter()
        .position(|event| matches!(event.payload, ThreadEventPayload::InputSubmitted { .. }))
        .expect("the input is durable");
    assert!(
        dispatched < submitted,
        "the input was written before the dispatch that owed it"
    );

    let submitted = inputs(&log);
    assert_eq!(
        submitted[0].1.as_deref(),
        Some(dispatch_message_id(id, NOW + 30 * SECOND).as_str()),
        "the submission does not carry the derived key"
    );
    assert!(submitted[0].2.contains("arroser les plantes"));
    opened.handle.shutdown().await;
}

/// AC3. Resubmitting the key a dispatch already used returns the original
/// identifiers and opens no second turn: the existing `already_accepted` path,
/// reached from the public submit.
#[tokio::test]
async fn a_replayed_dispatch_key_returns_the_original_identifiers() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    clock.advance(30 * SECOND);
    assert!(settle(&store, has_input).await);

    let first = inputs(&events(&store).await);
    assert_eq!(first.len(), 1);

    let replayed = opened
        .handle
        .submit(Submission {
            text: "peu importe le texte".into(),
            client_message_id: Some(dispatch_message_id(id, NOW + 30 * SECOND)),
            origin: agent_runtime::thread::InputOrigin::Runtime,
        })
        .await
        .expect("a replay is accepted");
    assert_eq!(replayed.turn_id, first[0].0, "a second turn was opened");
    assert_eq!(
        inputs(&events(&store).await).len(),
        1,
        "the replay wrote a second input"
    );
    opened.handle.shutdown().await;
}

/// AC4, edge cases 14 and 33. A stop lands precisely between the durable
/// dispatch and the submission it owes: the log is built by hand with the first
/// and without the second. The reopening delivers it once, and a second
/// reopening delivers nothing more.
#[tokio::test]
async fn a_stop_between_the_write_and_the_submission_delivers_once_on_reopening() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    {
        let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
            .delivery(CompletionDelivery::Quiet)
            .open()
            .await;
        schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
        // The cut: the dispatch is written by hand, the submission never
        // happens, and the thread stops.
        opened
            .handle
            .journal()
            .record(ThreadEventPayload::ScheduleDispatched {
                schedule_id: id,
                accepted_at_ms: None,
            })
            .await
            .expect("the dispatch is durable");
        opened.handle.shutdown().await;
    }

    let before = events(&store).await;
    assert_eq!(dispatches(&before).len(), 1);
    assert!(!has_input(&before), "the cut left a submission behind");

    memory.reopen();
    let first = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .seed(2_000)
        .open()
        .await;
    assert!(
        settle(&store, has_input).await,
        "the reopening delivered nothing"
    );
    let delivered = inputs(&events(&store).await);
    assert_eq!(delivered.len(), 1, "the reopening delivered twice");
    assert_eq!(
        delivered[0].1.as_deref(),
        Some(dispatch_message_id(id, NOW + 30 * SECOND).as_str())
    );
    assert_eq!(
        dispatches(&events(&store).await).len(),
        1,
        "the reopening wrote a second dispatch"
    );
    first.handle.shutdown().await;

    memory.reopen();
    let second = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .seed(3_000)
        .open()
        .await;
    quiesce().await;
    assert_eq!(
        inputs(&events(&store).await).len(),
        1,
        "a second reopening delivered the reminder again"
    );
    second.handle.shutdown().await;
}

/// AC5, edge case 16. A reminder that comes due while a turn is running enters
/// by the STEERING path: it is attached to the running turn and opens no turn
/// of its own.
#[tokio::test]
async fn a_reminder_due_during_a_turn_enters_by_the_steering_path() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let (release, held) = tokio::sync::oneshot::channel();
    let started = Arc::new(AtomicUsize::new(0));
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .runner(Arc::new(HeldRunner {
            release: Mutex::new(Some(held)),
            started: Arc::clone(&started),
        }))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    let human = opened
        .handle
        .submit(Submission::new("commence quelque chose de long"))
        .await
        .expect("the human input is accepted");
    assert!(settle(&store, |_| started.load(Ordering::SeqCst) == 1).await);

    clock.advance(30 * SECOND);
    assert!(
        settle(&store, |log| inputs(log).len() == 2).await,
        "the reminder never reached the running turn"
    );
    let submitted = inputs(&events(&store).await);
    assert_eq!(
        submitted[1].0, human.turn_id,
        "the reminder opened a turn of its own instead of steering the running one"
    );
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "a second turn was started while the first was running"
    );

    let _ = release.send(());
    opened.handle.shutdown().await;
}

/// AC6, edge case 13. The store refuses the dispatch entry: nothing is
/// submitted, the reminder stays due, and the thread is still there.
#[tokio::test]
async fn a_write_failure_at_dispatch_submits_nothing_and_keeps_the_thread_up() {
    let inner = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    // The third append is the dispatch: `ThreadCreated`, then the creation,
    // then the entry this test refuses.
    let store = Arc::new(FailingThreadStore::new(
        Arc::clone(&inner),
        FailurePoint::before(StoreOperation::Append, 3, "disque plein"),
    )) as Arc<dyn ThreadStore>;
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    clock.advance(30 * SECOND);
    quiesce().await;

    let log = events(&inner).await;
    assert!(
        !has_dispatch(&log),
        "a refused write still left a dispatch entry"
    );
    assert!(!has_input(&log), "a refused write still submitted an input");
    assert!(
        !opened.handle.status().shutting_down,
        "a refused write stopped the thread"
    );
    opened.handle.shutdown().await;
}

/// AC7, edge case 17. The steering buffer of the running turn is full: the
/// dispatch is refused, nothing durable is written, and the reminder is still
/// active and overdue when the thread is reopened.
#[tokio::test]
async fn a_full_input_queue_refuses_the_dispatch_and_keeps_the_reminder_overdue() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let (release, held) = tokio::sync::oneshot::channel();
    let started = Arc::new(AtomicUsize::new(0));
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .runner(Arc::new(HeldRunner {
            release: Mutex::new(Some(held)),
            started: Arc::clone(&started),
        }))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    opened
        .handle
        .submit(Submission::new("commence"))
        .await
        .expect("the first input opens a turn");
    assert!(settle(&store, |_| started.load(Ordering::SeqCst) == 1).await);
    for index in 0..MAX_PENDING_INPUTS {
        opened
            .handle
            .steer(Submission::new(format!("encore {index}")), None)
            .await
            .expect("the steering buffer takes it");
    }

    clock.advance(30 * SECOND);
    quiesce().await;

    let log = events(&store).await;
    assert!(
        !has_dispatch(&log),
        "a refused dispatch still wrote a durable entry"
    );
    assert_eq!(
        inputs(&log).len(),
        MAX_PENDING_INPUTS + 1,
        "the refused reminder still became an input"
    );

    let _ = release.send(());
    opened.handle.shutdown().await;

    // Conserved: the reopening still holds it, and still calls it overdue.
    memory.reopen();
    let reopened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .seed(4_000)
        .delivery(CompletionDelivery::Quiet)
        .open()
        .await;
    let schedules = &reopened.handle.resumed().schedules;
    assert_eq!(schedules.active.len(), 1, "the reminder was lost");
    assert_eq!(
        schedules.active[0].state,
        agent_runtime::schedule::ScheduleState::Overdue
    );
    reopened.handle.shutdown().await;
}

// -- US-157: one budget, shared, and a refusal conserves -----------------

/// AC2. A spent budget opens no turn, writes no dispatch entry, and leaves the
/// reminder overdue.
#[tokio::test]
async fn a_spent_wake_budget_opens_no_turn_and_writes_no_dispatch() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);

    // Three recurring reminders, five minutes apart, spend the whole budget one
    // occurrence at a time.
    let id = ScheduleId::generate(&ids);
    schedule(
        &opened.handle,
        id,
        ScheduleRule::Every {
            first_at_ms: NOW + 5 * MINUTE,
            interval_seconds: 300,
        },
        NOW,
    )
    .await;

    for round in 1..=MAX_CONSECUTIVE_WAKES {
        clock.advance(5 * MINUTE);
        assert!(
            settle(&store, |log| inputs(log).len() == round).await,
            "occurrence {round} was not delivered"
        );
    }

    // The fourth occurrence finds the budget spent.
    clock.advance(5 * MINUTE);
    quiesce().await;

    let log = events(&store).await;
    assert_eq!(
        inputs(&log).len(),
        MAX_CONSECUTIVE_WAKES,
        "the spent budget still opened a turn"
    );
    assert_eq!(
        dispatches(&log).len(),
        MAX_CONSECUTIVE_WAKES,
        "the spent budget still wrote a dispatch entry"
    );
    opened.handle.shutdown().await;
}

/// AC3. A human input rearms the budget, and only then is the conserved
/// reminder delivered. The order is the assertion.
#[tokio::test]
async fn a_human_input_rearms_the_budget_and_the_reminder_is_then_delivered() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::Every {
            first_at_ms: NOW + 5 * MINUTE,
            interval_seconds: 300,
        },
        NOW,
    )
    .await;
    for round in 1..=MAX_CONSECUTIVE_WAKES {
        clock.advance(5 * MINUTE);
        assert!(settle(&store, |log| inputs(log).len() == round).await);
    }
    clock.advance(5 * MINUTE);
    quiesce().await;
    let refused = inputs(&events(&store).await).len();
    assert_eq!(refused, MAX_CONSECUTIVE_WAKES, "the budget did not refuse");

    opened
        .handle
        .submit(Submission::new("me revoila"))
        .await
        .expect("a human input is accepted");
    assert!(
        settle(&store, |log| dispatches(log).len()
            == MAX_CONSECUTIVE_WAKES + 1)
        .await,
        "the conserved reminder was not delivered after the budget rearmed"
    );
    let delivered = inputs(&events(&store).await);
    assert_eq!(
        delivered[refused].2, "me revoila",
        "the human input did not come first"
    );
    assert!(
        delivered[refused + 1].2.contains("arroser les plantes"),
        "the reminder did not follow the human input"
    );
    opened.handle.shutdown().await;
}

/// AC4. Two job completions and two reminders alternating on an idle thread
/// open AT MOST three turns without an intervening human input. The budget is
/// one, and this is the test that would fail if a second one were introduced.
#[tokio::test]
async fn two_completions_and_two_reminders_open_at_most_three_turns() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::Every {
            first_at_ms: NOW + 5 * MINUTE,
            interval_seconds: 300,
        },
        NOW,
    )
    .await;

    // A completion, a reminder, a completion, a reminder.
    for round in 0..2 {
        let job = opened
            .jobs
            .register(JobKind::Terminal, format!("cargo test {round}"), 0)
            .await
            .expect("the job is registered");
        opened
            .jobs
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job settles");
        quiesce().await;
        clock.advance(5 * MINUTE);
        quiesce().await;
    }

    let opened_turns = inputs(&events(&store).await).len();
    assert!(
        opened_turns <= MAX_CONSECUTIVE_WAKES,
        "four unrequested producers opened {opened_turns} turns, past the shared budget of {MAX_CONSECUTIVE_WAKES}"
    );
    opened.handle.shutdown().await;
}

/// AC5, edge case 18. A recurring reminder a day late on a thread whose budget
/// is spent delivers ONE occurrence when the budget rearms: conservation and
/// last-occurrence-only compose, and the backlog is never enumerated.
#[tokio::test]
async fn a_recurring_reminder_a_day_late_delivers_one_occurrence_when_the_budget_rearms() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(
        &opened.handle,
        id,
        ScheduleRule::Every {
            first_at_ms: NOW + 5 * MINUTE,
            interval_seconds: 300,
        },
        NOW,
    )
    .await;
    for round in 1..=MAX_CONSECUTIVE_WAKES {
        clock.advance(5 * MINUTE);
        assert!(settle(&store, |log| inputs(log).len() == round).await);
    }

    // A full day passes with the budget spent: two hundred and eighty-eight
    // occurrences are missed and none of them is enumerated.
    clock.advance(DAY);
    quiesce().await;
    let refused = inputs(&events(&store).await).len();
    assert_eq!(refused, MAX_CONSECUTIVE_WAKES);

    opened
        .handle
        .submit(Submission::new("me revoila"))
        .await
        .expect("a human input is accepted");
    assert!(settle(&store, |log| dispatches(log).len() > MAX_CONSECUTIVE_WAKES).await);
    quiesce().await;

    let log = events(&store).await;
    assert_eq!(
        dispatches(&log).len(),
        MAX_CONSECUTIVE_WAKES + 1,
        "a day of backlog produced more than one occurrence"
    );
    let delivered = inputs(&log);
    assert_eq!(
        delivered[refused].2, "me revoila",
        "the human input did not come first"
    );
    assert_eq!(
        delivered[refused + 1].1.as_deref(),
        Some(dispatch_message_id(id, NOW + DAY + 15 * MINUTE).as_str()),
        "the delivery replayed a slot from the backlog instead of the last one"
    );
    opened.handle.shutdown().await;
}

/// AC6, edge case 30. Under `Quiet`, which is what `-p` and the app-server
/// take, an echeance opens nothing at all and the reminder stays due. The
/// dispatch entry is never written either, so the next interactive opening
/// still owes it.
#[tokio::test]
async fn a_quiet_client_opens_no_turn_and_leaves_the_reminder_due() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .delivery(CompletionDelivery::Quiet)
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    clock.advance(30 * SECOND);
    quiesce().await;

    let log = events(&store).await;
    assert!(!has_input(&log), "a quiet client opened a turn");
    assert!(!has_dispatch(&log), "a quiet client spent the reminder");
    opened.handle.shutdown().await;

    memory.reopen();
    let reopened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .seed(5_000)
        .delivery(CompletionDelivery::Quiet)
        .open()
        .await;
    let schedules = &reopened.handle.resumed().schedules;
    assert_eq!(schedules.active.len(), 1, "the reminder was lost");
    assert_eq!(
        schedules.active[0].state,
        agent_runtime::schedule::ScheduleState::Overdue
    );
    reopened.handle.shutdown().await;
}

/// Edge case 34. Two recurring reminders that come due together are ONE
/// submission: two dispatch entries, one input, one turn.
#[tokio::test]
async fn two_recurring_reminders_due_together_are_one_submission() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let first = ScheduleId::generate(&ids);
    let second = ScheduleId::generate(&ids);

    for id in [first, second] {
        schedule(
            &opened.handle,
            id,
            ScheduleRule::Every {
                first_at_ms: NOW + 5 * MINUTE,
                interval_seconds: 300,
            },
            NOW,
        )
        .await;
    }

    clock.advance(5 * MINUTE);
    assert!(settle(&store, has_input).await);
    quiesce().await;

    let log = events(&store).await;
    assert_eq!(dispatches(&log), vec![first, second]);
    let submitted = inputs(&log);
    assert_eq!(submitted.len(), 1, "a batch produced two submissions");
    assert_eq!(
        submitted[0].1.as_deref(),
        Some(dispatch_message_id(first, NOW + 5 * MINUTE).as_str()),
        "the batch did not take the key of its first occurrence"
    );
    opened.handle.shutdown().await;
}

/// AC3, the re-admission edge. A reminder steered into a turn that reaches its
/// terminal before consuming it comes back as its own turn, key included, and
/// the acceptance map must still answer with the identifiers the FIRST
/// acceptance was given: a replaying client and a restarted process both read
/// the same answer, which is what invariant 12 asks for. `resume.rs` folds the
/// log by first-acceptance-wins, so the live map may not overwrite.
#[tokio::test]
async fn a_re_admitted_reminder_still_replays_to_its_first_acceptance() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let clock = TestClock::at(NOW);
    let (release, held) = tokio::sync::oneshot::channel();
    let started = Arc::new(AtomicUsize::new(0));
    let opened = Setup::new(Arc::clone(&store), Arc::clone(&clock))
        .runner(Arc::new(HeldRunner {
            release: Mutex::new(Some(held)),
            started: Arc::clone(&started),
        }))
        .open()
        .await;
    let ids = SequentialIds::starting_at(900);
    let id = ScheduleId::generate(&ids);

    schedule(&opened.handle, id, ScheduleRule::After { seconds: 30 }, NOW).await;
    let human = opened
        .handle
        .submit(Submission::new("commence quelque chose de long"))
        .await
        .expect("the human input is accepted");
    assert!(settle(&store, |_| started.load(Ordering::SeqCst) == 1).await);

    clock.advance(30 * SECOND);
    assert!(
        settle(&store, |log| inputs(log).len() == 2).await,
        "the reminder never reached the running turn"
    );

    // The held runner never drained its input queue, so ending the turn is what
    // re-admits the reminder under a turn of its own.
    let _ = release.send(());
    assert!(
        settle(&store, |log| inputs(log).len() == 3).await,
        "the unconsumed reminder was never re-admitted"
    );
    let submitted = inputs(&events(&store).await);
    let key = dispatch_message_id(id, NOW + 30 * SECOND);
    assert_eq!(submitted[1].1.as_deref(), Some(key.as_str()));
    assert_eq!(
        submitted[2].1.as_deref(),
        Some(key.as_str()),
        "the re-admission dropped the idempotency key"
    );
    assert_ne!(
        submitted[2].0, human.turn_id,
        "the re-admission stayed on the terminal turn"
    );

    let replayed = opened
        .handle
        .submit(Submission {
            text: "peu importe le texte".into(),
            client_message_id: Some(key),
            origin: agent_runtime::thread::InputOrigin::Runtime,
        })
        .await
        .expect("a replay is accepted");
    assert_eq!(
        replayed.turn_id, human.turn_id,
        "the replay was answered with the re-admitted turn instead of the first acceptance"
    );
    assert_eq!(
        inputs(&events(&store).await).len(),
        3,
        "the replay wrote a fourth input"
    );
    opened.handle.shutdown().await;
}
