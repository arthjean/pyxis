//! The registry seen from the thread that owns it (EP-041).
//!
//! Everything below goes through [`ThreadHandle::start`]: the registry is bound
//! to a real actor, its writes go through the real mailbox, and its teardown is
//! the real shutdown. What is asserted is the DURABLE log, because that is what
//! a reopened thread reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::id::{JobId, SequentialIds, ThreadId, TurnId};
use agent_runtime::jobs::{
    CompletionDelivery, JobError, JobKind, JobLaunch, JobLauncher, JobOutcome, JobProcess,
    JobRegistry, JobStatus, MAX_ACTIVE_JOBS, MAX_CONSECUTIVE_WAKES, RESTART_CAUSE, TEARDOWN_CAUSE,
};
use agent_runtime::runner::{TurnOutcome, TurnRequest, TurnRunner};
use agent_runtime::store::{
    FailingThreadStore, FailurePoint, MemoryThreadStore, StoreOperation, ThreadStore,
};
use agent_runtime::thread::{ThreadHandle, ThreadOptions};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A runner no test in this file ever reaches: a background job is registered
/// by a tool, not by a turn, and none of these assertions needs a model.
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

struct FrozenClock;

#[async_trait::async_trait]
impl Clock for FrozenClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
    async fn sleep(&self, _dur: Duration) {}
}

/// A launcher with nothing behind it. These tests assert the durable log, and a
/// log does not care whether a process exists; what it needs is a registry that
/// accepts registrations, which one without a launcher refuses.
///
/// It counts its calls because US-145 AC5 is a NEGATIVE claim: a resume starts
/// no process. The only way to prove that from the outside is to hold the one
/// door a process can come through and read it as still shut.
struct TestLauncher {
    launches: Arc<AtomicUsize>,
}

impl TestLauncher {
    fn shared() -> Arc<dyn JobLauncher> {
        Self::counting(Arc::new(AtomicUsize::new(0)))
    }

    fn counting(launches: Arc<AtomicUsize>) -> Arc<dyn JobLauncher> {
        Arc::new(Self { launches }) as Arc<dyn JobLauncher>
    }
}

#[async_trait::async_trait]
impl JobLauncher for TestLauncher {
    async fn launch(&self, _launch: JobLaunch) -> Result<Arc<dyn JobProcess>, String> {
        self.launches.fetch_add(1, Ordering::SeqCst);
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
            max_pending_inputs: 16,
        },
    }
}

struct Owner {
    handle: ThreadHandle,
    jobs: Arc<JobRegistry>,
    root: CancellationToken,
}

/// Opens a thread that owns a registry, on the store the caller hands over.
///
/// `Quiet` unless a test says otherwise: EP-041 and EP-042 assert the durable
/// log of a registry, and a completion opening a turn is EP-044's subject.
async fn open(store: Arc<dyn ThreadStore>, seed: u64) -> Owner {
    open_with(store, seed, CompletionDelivery::Quiet).await
}

async fn open_with(store: Arc<dyn ThreadStore>, seed: u64, delivery: CompletionDelivery) -> Owner {
    open_full(store, seed, delivery, Arc::new(IdleRunner)).await
}

async fn open_full(
    store: Arc<dyn ThreadStore>,
    seed: u64,
    delivery: CompletionDelivery,
    runner: Arc<dyn TurnRunner>,
) -> Owner {
    open_launched(store, seed, delivery, runner, TestLauncher::shared()).await
}

async fn open_launched(
    store: Arc<dyn ThreadStore>,
    seed: u64,
    delivery: CompletionDelivery,
    runner: Arc<dyn TurnRunner>,
    launcher: Arc<dyn JobLauncher>,
) -> Owner {
    let ids = Arc::new(SequentialIds::starting_at(seed));
    let thread_id = store
        .read()
        .await
        .expect("the store reads")
        .thread_id
        .unwrap_or_else(|| ThreadId::generate(ids.as_ref()));
    let jobs = JobRegistry::new(
        Arc::clone(&ids) as Arc<_>,
        Arc::new(FrozenClock),
        Some(launcher),
    );
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store: Arc::clone(&store),
        runner,
        turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
            ids.as_ref(),
        )))) as Arc<dyn TurnContextSource>,
        ids,
        clock: Arc::new(FrozenClock),
        parent_cancel: root.clone(),
        agents: None,
        jobs: Some(Arc::clone(&jobs)),
        completion_delivery: delivery,
    })
    .await
    .expect("the thread starts");
    Owner { handle, jobs, root }
}

/// Everything the durable log says about jobs, in order, as a readable trace.
async fn job_trace(store: &Arc<dyn ThreadStore>) -> Vec<(JobId, String)> {
    store
        .read()
        .await
        .expect("the store reads")
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::JobRegistered { job_id, .. } => {
                Some((*job_id, "registered".to_string()))
            }
            ThreadEventPayload::JobStateChanged { job_id, to, .. } => {
                Some((*job_id, to.as_str().to_string()))
            }
            ThreadEventPayload::JobReported { job_id } => Some((*job_id, "reported".to_string())),
            _ => None,
        })
        .collect()
}

fn steps(trace: &[(JobId, String)], job_id: JobId) -> Vec<&str> {
    trace
        .iter()
        .filter(|(id, _)| *id == job_id)
        .map(|(_, step)| step.as_str())
        .collect()
}

/// EP-041, definition of done: a job registered and settled in one run is found
/// again, with its terminal state and its report flag, by a thread reopened on
/// the same log. No process is restarted.
#[tokio::test]
async fn a_reopened_thread_finds_its_jobs_their_terminal_state_and_their_flag() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    let exited = first
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");
    let stopped = first
        .jobs
        .register(JobKind::Terminal, "npm run dev", 0)
        .await
        .expect("the registration is accepted");
    first
        .jobs
        .settle(exited.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");
    first
        .jobs
        .cancel(stopped.job_id, TEARDOWN_CAUSE)
        .await
        .expect("the job is known");
    first.handle.shutdown().await;

    // The durable half of a restart: the first process closed its log, the
    // state it wrote is still there, and a second process opens the same file.
    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;
    let rebuilt = second.jobs.snapshots();

    assert_eq!(rebuilt.len(), 2, "both jobs came back");
    let exited_again = &rebuilt[0];
    assert_eq!(exited_again.job_id, exited.job_id);
    assert_eq!(exited_again.command, "npm test");
    assert_eq!(exited_again.status, JobStatus::Completed);
    assert_eq!(exited_again.exit_code, Some(0));
    assert!(
        !exited_again.reported,
        "a finished job may still be unreported"
    );
    let stopped_again = &rebuilt[1];
    assert_eq!(stopped_again.status, JobStatus::Killed);
    assert!(
        stopped_again.reported,
        "a reported job is necessarily finished"
    );
    assert_eq!(stopped_again.cause.as_deref(), Some(TEARDOWN_CAUSE));
    assert!(
        rebuilt.iter().all(|job| !job.attached),
        "nothing was relaunched"
    );
    assert_eq!(second.jobs.active(), 0);
    second.handle.shutdown().await;
}

/// The undurable half of a restart: the process VANISHES.
///
/// Forgetting the handle is what makes this a crash and not a shutdown. Dropping
/// it would close the mailbox, the actor would reach its teardown, and every
/// active job would already carry `TEARDOWN_CAUSE` before the next run ever read
/// the log. A crash writes nothing, and a log a crash left behind is exactly the
/// one US-145 has to repair.
fn crash(owner: Owner) {
    std::mem::forget(owner.handle);
}

/// US-145 AC1, AC5, AC7: reopening a log that still says `running` closes every
/// active job with a durable cause, and starts nothing.
#[tokio::test]
async fn a_resume_closes_every_active_job_with_a_durable_cause() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let launched = Arc::new(AtomicUsize::new(0));
    let first = open_launched(
        Arc::clone(&store),
        1,
        CompletionDelivery::Quiet,
        Arc::new(IdleRunner),
        TestLauncher::counting(Arc::clone(&launched)),
    )
    .await;
    let watched = first
        .jobs
        .register(JobKind::Terminal, "npm run dev", 7)
        .await
        .expect("the registration is accepted");
    let building = first
        .jobs
        .register(JobKind::Terminal, "cargo build", 8)
        .await
        .expect("the registration is accepted");
    assert_eq!(
        launched.load(Ordering::SeqCst),
        2,
        "the first run really started both processes"
    );
    crash(first);

    memory.reopen();
    let relaunched = Arc::new(AtomicUsize::new(0));
    let second = open_launched(
        Arc::clone(&store),
        500,
        CompletionDelivery::Quiet,
        Arc::new(IdleRunner),
        TestLauncher::counting(Arc::clone(&relaunched)),
    )
    .await;

    let rebuilt = second.jobs.snapshots();
    assert_eq!(rebuilt.len(), 2, "both jobs came back");
    for job in &rebuilt {
        assert_eq!(job.status, JobStatus::Killed, "{} was closed", job.command);
        assert_eq!(job.cause.as_deref(), Some(RESTART_CAUSE));
        assert!(job.ended_at_ms.is_some(), "a closed job has an end date");
        assert!(!job.attached, "nothing was re-attached");
    }
    assert!(
        !rebuilt
            .iter()
            .any(|job| job.status == JobStatus::Running || job.status == JobStatus::Stopping),
        "no active state survives a resume"
    );
    assert_eq!(second.jobs.active(), 0, "no slot is held by the dead run");
    assert_eq!(
        relaunched.load(Ordering::SeqCst),
        0,
        "a resume reports, it does not relaunch"
    );
    // US-146 AC2: what THIS resume closed, which is what the human is told.
    let named: Vec<String> = second
        .jobs
        .interrupted_by_restart()
        .into_iter()
        .map(|job| job.command)
        .collect();
    assert_eq!(named, ["npm run dev", "cargo build"]);

    // The repair is DURABLE, not a projection the next reader would have to
    // redo: the log itself carries the closing transition.
    let trace = job_trace(&store).await;
    assert_eq!(steps(&trace, watched.job_id), vec!["registered", "killed"]);
    assert_eq!(steps(&trace, building.job_id), vec!["registered", "killed"]);
    second.handle.shutdown().await;
}

/// US-145 AC2: the reconciliation is written once. A second restart reads a
/// terminal record and has nothing left to repair.
#[tokio::test]
async fn a_second_resume_does_not_rewrite_the_reconciliation() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    let job = first
        .jobs
        .register(JobKind::Terminal, "npm run dev", 7)
        .await
        .expect("the registration is accepted");
    crash(first);

    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;
    let after_first_resume = second.jobs.get(job.job_id).expect("the job came back");
    // US-146 AC2: the resume that CLOSED the job is the one that names it.
    assert_eq!(second.jobs.interrupted_by_restart().len(), 1);
    crash(second);

    memory.reopen();
    let third = open(Arc::clone(&store), 900).await;
    let after_second_resume = third.jobs.get(job.job_id).expect("the job came back");

    assert_eq!(
        after_second_resume, after_first_resume,
        "the second resume read the repair, it did not redo it"
    );
    // US-146 AC2, the half the durable cause cannot answer: the SECOND resume
    // interrupted nothing, so it names nothing. The record still carries
    // `RESTART_CAUSE`, which is exactly why the selection is the registry's and
    // not a filter a surface could rebuild from a snapshot.
    assert_eq!(
        after_second_resume.cause.as_deref(),
        Some(RESTART_CAUSE),
        "the durable cause outlives the resume that wrote it"
    );
    assert!(
        third.jobs.interrupted_by_restart().is_empty(),
        "a resume that closed nothing has nothing to announce"
    );
    let trace = job_trace(&store).await;
    assert_eq!(
        steps(&trace, job.job_id),
        vec!["registered", "killed"],
        "one reconciliation transition, not one per restart"
    );
    third.handle.shutdown().await;
}

/// US-145 AC3: a job the log already left terminal is re-read as it stands. No
/// transition is written for it, and its report flag is not touched.
#[tokio::test]
async fn a_resume_writes_nothing_for_a_job_that_was_already_terminal() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    let job = first
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");
    first
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");
    first
        .jobs
        .mark_reported(job.job_id)
        .await
        .expect("the job is known");
    let before = job_trace(&store).await;
    crash(first);

    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;
    let rebuilt = second.jobs.get(job.job_id).expect("the job came back");

    assert_eq!(rebuilt.status, JobStatus::Completed, "the state is re-read");
    assert_eq!(rebuilt.exit_code, Some(0));
    assert_eq!(rebuilt.cause, None, "no restart cause was grafted onto it");
    assert!(rebuilt.reported, "the flag is re-read too");
    assert_eq!(
        job_trace(&store).await,
        before,
        "a terminal job costs the resume no entry"
    );
    second.handle.shutdown().await;
}

/// US-145 AC4: a job that finished while nobody was reading stays owed an
/// announcement across the restart, and is announced exactly once.
///
/// This is the deliberate divergence from `AgentSupervisor::restore`, which
/// marks a restored handoff `delivered: true`. A handoff is a message the parent
/// already had its chance to read in the run that produced it; a background job
/// is the opposite case, since the whole reason the registry exists is that
/// nobody was watching when the job settled.
#[tokio::test]
async fn a_job_finished_before_the_crash_is_still_owed_its_announcement() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    let job = first
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");
    first
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");
    crash(first);

    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;
    let owed = second.jobs.unreported();
    assert_eq!(owed.len(), 1, "the restart owes exactly one announcement");
    assert_eq!(owed[0].job_id, job.job_id);
    second
        .jobs
        .mark_reported(job.job_id)
        .await
        .expect("the job is known");
    crash(second);

    memory.reopen();
    let third = open(Arc::clone(&store), 900).await;
    assert!(
        third.jobs.unreported().is_empty(),
        "an announced job is never announced a second time"
    );
    assert_eq!(
        steps(&job_trace(&store).await, job.job_id),
        vec!["registered", "completed", "reported"],
        "neither restart added a transition of its own"
    );
    third.handle.shutdown().await;
}

/// US-145 AC6, edge case #11: a log written before this lot names no job. It
/// reopens empty, with no error and nothing to migrate.
#[tokio::test]
async fn a_log_that_predates_the_registry_reopens_with_no_job() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    first
        .handle
        .submit(agent_runtime::thread::Submission::new("hello"))
        .await
        .expect("the submission is accepted");
    first.handle.shutdown().await;
    assert!(
        job_trace(&store).await.is_empty(),
        "this log knows nothing about jobs"
    );

    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;

    assert!(second.jobs.snapshots().is_empty(), "no job was invented");
    assert_eq!(second.jobs.active(), 0);
    assert!(
        job_trace(&store).await.is_empty(),
        "and none was written to repair a history that has none"
    );
    second.handle.shutdown().await;
}

/// US-133 AC5: the terminal transition is in the log before the caller that
/// would announce it holds anything to announce.
#[tokio::test]
async fn a_terminal_reaches_the_log_before_the_settle_answers() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open(Arc::clone(&store), 1).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");

    let settled = owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 3 })
        .await
        .expect("the job is known")
        .expect("the first settle wins");

    assert_eq!(settled.status, JobStatus::Completed);
    assert_eq!(
        steps(&job_trace(&store).await, job.job_id),
        vec!["registered", "completed"],
        "the durable log already carries the terminal"
    );
    owner.handle.shutdown().await;
}

/// US-133 AC4: a journal write cut at the registration refuses, registers
/// nothing and leaves nothing to launch.
#[tokio::test]
async fn a_registration_whose_log_write_is_cut_refuses_and_registers_nothing() {
    // Append 1 is `thread_created`; append 2 is the registration itself.
    let store = Arc::new(FailingThreadStore::new(
        Arc::new(MemoryThreadStore::new()),
        FailurePoint::before(StoreOperation::Append, 2, "cut at the registration"),
    )) as Arc<dyn ThreadStore>;
    let owner = open(Arc::clone(&store), 1).await;

    let refused = owner
        .jobs
        .register(JobKind::Terminal, "npm run dev", 0)
        .await
        .expect_err("a registration the log refuses is refused");

    assert!(matches!(refused, JobError::NotRegistered(_)));
    assert!(owner.jobs.snapshots().is_empty());
    assert!(
        job_trace(&store).await.is_empty(),
        "nothing about a job reached the log"
    );
    owner.handle.shutdown().await;
}

/// US-132 AC3: a job belongs to ONE thread. From another thread it is not
/// refused, it is absent.
#[tokio::test]
async fn a_job_of_one_thread_is_invisible_from_another() {
    let mine = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let theirs = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open(Arc::clone(&mine), 1).await;
    let stranger = open(Arc::clone(&theirs), 500).await;

    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm run dev", 0)
        .await
        .expect("the registration is accepted");

    assert!(stranger.jobs.get(job.job_id).is_none());
    assert!(stranger.jobs.snapshots().is_empty());
    assert_eq!(
        stranger
            .jobs
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .err(),
        Some(JobError::Unknown { job_id: job.job_id })
    );
    assert_eq!(
        stranger.jobs.cancel(job.job_id, TEARDOWN_CAUSE).await.err(),
        Some(JobError::Unknown { job_id: job.job_id })
    );
    assert_eq!(
        owner.jobs.get(job.job_id).map(|snapshot| snapshot.status),
        Some(JobStatus::Running),
        "the owner's job was not touched"
    );
    assert!(job_trace(&theirs).await.is_empty());
    owner.handle.shutdown().await;
    stranger.handle.shutdown().await;
}

/// US-134 AC1/AC2/AC5: a thread that stops takes its jobs with it. Each active
/// job goes through `stopping` then `killed`, both durable, inside the shutdown
/// budget the thread already had.
#[tokio::test]
async fn a_thread_shutdown_stops_every_job_and_the_log_carries_both_states() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open(Arc::clone(&store), 1).await;
    let mut running = Vec::new();
    for index in 0..MAX_ACTIVE_JOBS {
        running.push(
            owner
                .jobs
                .register(JobKind::Terminal, format!("tail -f {index}.log"), 0)
                .await
                .expect("a slot is free"),
        );
    }

    let started = std::time::Instant::now();
    owner.handle.shutdown().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < agent_runtime::thread::SHUTDOWN_DEADLINE,
        "the jobs were stopped inside the existing deadline, not after a second one"
    );
    let trace = job_trace(&store).await;
    for job in &running {
        assert!(
            job.cancel.is_cancelled(),
            "every job hangs from the thread's node"
        );
        assert_eq!(
            steps(&trace, job.job_id),
            vec!["registered", "reported", "stopping", "killed"],
            "the flag comes first, then both states, all durable"
        );
    }
    assert_eq!(owner.jobs.active(), 0);
    assert!(!owner.root.is_cancelled(), "cancellation never climbs");
}

/// US-134 AC6: an interruption landing DURING a registration leaves the
/// registry coherent. Once the thread is down, either the job is not there at
/// all, or it is there and it is settled. Never a record whose process nobody
/// will start.
#[tokio::test]
async fn an_interruption_during_a_registration_leaves_the_registry_coherent() {
    for round in 0..32u64 {
        let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
        let owner = open(Arc::clone(&store), round * 100 + 1).await;
        let jobs = Arc::clone(&owner.jobs);
        let registering =
            tokio::spawn(async move { jobs.register(JobKind::Terminal, "npm run dev", 0).await });
        // The interruption lands at a different point of the registration each
        // round, without a single sleep.
        for _ in 0..(round % 5) {
            tokio::task::yield_now().await;
        }
        owner.root.cancel();
        let registered = registering.await.expect("the registration task finishes");
        owner.handle.shutdown().await;

        let trace = job_trace(&store).await;
        match registered {
            Ok(job) => {
                let snapshot = owner
                    .jobs
                    .get(job.job_id)
                    .expect("an accepted registration is in the registry");
                assert!(
                    snapshot.status.is_terminal(),
                    "round {round}: an accepted job outlived its thread as `{}`",
                    snapshot.status.as_str()
                );
                assert!(job.cancel.is_cancelled());
                assert_eq!(
                    steps(&trace, job.job_id).first(),
                    Some(&"registered"),
                    "round {round}: an accepted registration is in the log"
                );
                assert!(
                    steps(&trace, job.job_id).contains(&"killed"),
                    "round {round}: its terminal is in the log too"
                );
            }
            Err(err) => {
                assert!(
                    matches!(err, JobError::NotRegistered(_)),
                    "round {round}: a refused registration names why: {err}"
                );
                assert!(owner.jobs.snapshots().is_empty());
                assert!(trace.is_empty(), "round {round}: a refusal wrote nothing");
            }
        }
    }
}

/// US-140 AC3 and AC5: `unreported` names the finished jobs nothing announced,
/// the flag flips exactly once, and a thread reopened on the same log reads the
/// flag back instead of re-announcing what was already delivered.
#[tokio::test]
async fn the_report_flag_is_written_once_and_read_back_by_a_reopened_thread() {
    let memory = Arc::new(MemoryThreadStore::new());
    let store = Arc::clone(&memory) as Arc<dyn ThreadStore>;
    let first = open(Arc::clone(&store), 1).await;
    let read = first
        .jobs
        .register(JobKind::Terminal, "cargo build", 0)
        .await
        .expect("the registration is accepted");
    let unread = first
        .jobs
        .register(JobKind::Terminal, "cargo test", 0)
        .await
        .expect("the registration is accepted");
    for job in [read.job_id, unread.job_id] {
        first
            .jobs
            .settle(job, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");
    }

    assert!(
        first.jobs.mark_reported(read.job_id).await.expect("known"),
        "the first collection is the flip"
    );
    assert!(
        !first.jobs.mark_reported(read.job_id).await.expect("known"),
        "reading it again announces nothing"
    );
    assert_eq!(
        first
            .jobs
            .unreported()
            .iter()
            .map(|job| job.job_id)
            .collect::<Vec<_>>(),
        vec![unread.job_id],
        "only what nobody read"
    );
    first.handle.shutdown().await;

    memory.reopen();
    let second = open(Arc::clone(&store), 500).await;
    let trace = job_trace(&store).await;
    assert_eq!(
        steps(&trace, read.job_id),
        vec!["registered", "completed", "reported"],
        "one durable line for the flip, and only one"
    );
    assert_eq!(
        steps(&trace, unread.job_id),
        vec!["registered", "completed"]
    );
    assert_eq!(
        second
            .jobs
            .unreported()
            .iter()
            .map(|job| job.job_id)
            .collect::<Vec<_>>(),
        vec![unread.job_id],
        "the reopened thread does not re-announce a delivered result"
    );
    second.handle.shutdown().await;
    drop(first.root);
    drop(second.root);
}

// ───────── EP-044: the completion that comes back on its own ─────────

/// A runner that parks a turn until the test releases it, so a completion can
/// land while a turn is genuinely in progress rather than between two.
struct ParkedRunner {
    started: mpsc::Sender<()>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl TurnRunner for ParkedRunner {
    async fn run_turn(
        &self,
        _request: TurnRequest,
        _events: mpsc::Sender<AgentEvent>,
        _cancel: CancellationToken,
    ) -> TurnOutcome {
        let _ = self.started.send(()).await;
        self.release.notified().await;
        TurnOutcome::Completed
    }
}

/// Every input the durable log carries, oldest first.
async fn inputs(store: &Arc<dyn ThreadStore>) -> Vec<(TurnId, Option<String>, String)> {
    store
        .read()
        .await
        .expect("the store reads")
        .events
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

/// The idempotency key EP-044 derives from a job identifier. Spelled out here
/// rather than imported: it is a CONTRACT the log carries across restarts, so a
/// change to it has to break a test and not merely follow one.
fn delivery_key(job_id: JobId) -> String {
    format!("job-completion:{job_id}")
}

/// US-142 AC2 and AC5: an idle thread whose job settles unreported opens a turn
/// through the ordinary submit path, and the input that opens it is keyed on
/// the job.
#[tokio::test]
async fn a_completion_nobody_relieved_opens_a_turn_keyed_on_the_job() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");

    owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 1 })
        .await
        .expect("the job is known")
        .expect("the first settle wins");

    let submitted = inputs(&store).await;
    assert_eq!(submitted.len(), 1, "exactly one turn was opened");
    let (_, key, text) = &submitted[0];
    assert_eq!(key.as_deref(), Some(delivery_key(job.job_id).as_str()));
    assert!(
        text.contains(&job.job_id.to_string()) && text.contains("exit_code=1"),
        "the announcement names the job and its exit code: {text}"
    );
    assert!(
        text.contains("npm test") && text.contains("list_jobs"),
        "the announcement is an index into list_jobs: {text}"
    );
    assert!(
        owner.jobs.unreported().is_empty(),
        "an announced job owes no second announcement"
    );

    // AC6: the announcement follows the commit. The terminal transition is in
    // the log BEFORE the input that announces it.
    let trace = job_trace(&store).await;
    assert_eq!(
        steps(&trace, job.job_id),
        vec!["registered", "completed", "reported"]
    );
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-142 AC5, second half: replaying the key a delivery used returns the
/// original identifiers and opens nothing. Proved through the real `submit`,
/// which is the path a redelivery takes (invariant 12).
#[tokio::test]
async fn a_replayed_delivery_returns_the_original_identifiers_and_opens_no_second_turn() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");
    owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");
    let first = inputs(&store).await;
    let (opened_turn, _, announcement) = first[0].clone();

    let replayed = owner
        .handle
        .submit(agent_runtime::thread::Submission {
            text: announcement,
            client_message_id: Some(delivery_key(job.job_id)),
            origin: agent_runtime::thread::InputOrigin::Human,
        })
        .await
        .expect("a replay is accepted, not refused");

    assert_eq!(
        replayed.turn_id, opened_turn,
        "the replay answers with the turn the first delivery opened"
    );
    assert_eq!(
        inputs(&store).await.len(),
        1,
        "nothing was executed a second time"
    );
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-142 AC3: a completion that lands while a turn runs enters that turn as a
/// steer, at its next safe point, and spends no wake.
#[tokio::test]
async fn a_completion_landing_during_a_turn_enters_it_as_a_steer() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let (started_tx, mut started_rx) = mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let owner = open_full(
        Arc::clone(&store),
        1,
        CompletionDelivery::Wake,
        Arc::new(ParkedRunner {
            started: started_tx,
            release: Arc::clone(&release),
        }),
    )
    .await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");

    let human = owner
        .handle
        .submit(agent_runtime::thread::Submission::new("what is going on"))
        .await
        .expect("the submission is accepted");
    started_rx.recv().await.expect("the turn is running");

    owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");

    let submitted = inputs(&store).await;
    assert_eq!(submitted.len(), 2, "the human input and the announcement");
    let (announced_turn, key, _) = &submitted[1];
    assert_eq!(key.as_deref(), Some(delivery_key(job.job_id).as_str()));
    assert_eq!(
        *announced_turn, human.turn_id,
        "the announcement joined the running turn instead of opening one"
    );
    release.notify_waiters();
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-142 AC4: past the budget nothing is opened, and the job stays unreported
/// so the end-of-turn notice still names it.
#[tokio::test]
async fn the_wake_budget_stops_at_three_and_leaves_the_rest_unreported() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;

    let mut jobs = Vec::new();
    for index in 0..MAX_CONSECUTIVE_WAKES + 1 {
        let job = owner
            .jobs
            .register(JobKind::Terminal, format!("job {index}"), 0)
            .await
            .expect("the registration is accepted");
        owner
            .jobs
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");
        jobs.push(job.job_id);
    }

    assert_eq!(
        inputs(&store).await.len(),
        MAX_CONSECUTIVE_WAKES,
        "the fourth completion opened nothing"
    );
    let left = owner.jobs.unreported();
    assert_eq!(left.len(), 1, "one job is still owed an announcement");
    assert_eq!(left[0].job_id, jobs[MAX_CONSECUTIVE_WAKES]);
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-143 AC2 and AC3: a human message rearms the budget, and only a human
/// message does.
#[tokio::test]
async fn only_a_human_message_rearms_the_wake_budget() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;

    let settle_one = async |command: &str| {
        let job = owner
            .jobs
            .register(JobKind::Terminal, command, 0)
            .await
            .expect("the registration is accepted");
        owner
            .jobs
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");
        job.job_id
    };

    for index in 0..MAX_CONSECUTIVE_WAKES {
        settle_one(&format!("first wave {index}")).await;
    }
    let exhausted = settle_one("refused").await;
    assert_eq!(inputs(&store).await.len(), MAX_CONSECUTIVE_WAKES);

    owner
        .handle
        .submit(agent_runtime::thread::Submission::new("carry on"))
        .await
        .expect("the submission is accepted");
    let after_human = settle_one("rearmed").await;

    let keys: Vec<String> = inputs(&store)
        .await
        .into_iter()
        .filter_map(|(_, key, _)| key)
        .collect();
    assert!(
        keys.contains(&delivery_key(after_human)),
        "the completion after a human message opened a turn: {keys:?}"
    );
    assert!(
        !keys.contains(&delivery_key(exhausted)),
        "the one refused before the human message stays refused"
    );
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-143 AC5: a turn a completion opened cannot pass as human, so a chain of
/// completions runs the budget down instead of renewing it.
#[tokio::test]
async fn a_turn_opened_by_a_completion_does_not_rearm_the_budget() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;

    // Each job settles only once the previous one's turn is over, which is the
    // chain the budget is meant to stop: settle, wake, settle again.
    let mut opened = 0;
    for index in 0..MAX_CONSECUTIVE_WAKES * 2 {
        let job = owner
            .jobs
            .register(JobKind::Terminal, format!("chained {index}"), 0)
            .await
            .expect("the registration is accepted");
        owner
            .jobs
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");
        opened = inputs(&store).await.len();
    }

    assert_eq!(
        opened, MAX_CONSECUTIVE_WAKES,
        "the chain stopped at the budget instead of renewing it"
    );
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-142 AC7 and US-144 AC1: with the budget at zero, which is what
/// [`CompletionDelivery::Quiet`] sets it to, nothing is ever opened. The
/// accounting is unchanged and the job is simply left unreported, which is the
/// behavior every client had before EP-044.
#[tokio::test]
async fn a_quiet_client_opens_nothing_and_still_accounts_for_the_job() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Quiet).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");

    owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await
        .expect("the job is known");

    assert!(
        inputs(&store).await.is_empty(),
        "a quiet client is never woken"
    );
    let trace = job_trace(&store).await;
    assert_eq!(steps(&trace, job.job_id), vec!["registered", "completed"]);
    assert_eq!(
        owner.jobs.unreported().len(),
        1,
        "the completion is owed to the end-of-turn notice"
    );
    owner.handle.shutdown().await;
    owner.root.cancel();
}

/// US-142 AC8: a delivery whose durable write fails leaves the job unreported.
/// Never the inverse, because a job marked reported on a write nobody kept is a
/// result lost in silence.
#[tokio::test]
async fn a_delivery_whose_write_fails_leaves_the_job_unreported() {
    let memory = Arc::new(MemoryThreadStore::new());
    // The registration and the terminal transition go through first; the fourth
    // append is the input the announcement would open its turn with.
    let failing = Arc::new(FailingThreadStore::new(
        Arc::clone(&memory) as Arc<dyn ThreadStore>,
        FailurePoint::before(
            StoreOperation::Append,
            4,
            "the announcement could not be persisted",
        ),
    ));
    let store = Arc::clone(&failing) as Arc<dyn ThreadStore>;
    let owner = open_with(Arc::clone(&store), 1, CompletionDelivery::Wake).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test", 0)
        .await
        .expect("the registration is accepted");

    let _ = owner
        .jobs
        .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
        .await;

    // The fault fell on the announcement and not before it: the terminal
    // transition is durable, and no input opened a turn about it.
    let trace = job_trace(&store).await;
    assert_eq!(steps(&trace, job.job_id), vec!["registered", "completed"]);
    assert!(inputs(&store).await.is_empty(), "no turn was opened");
    assert_eq!(
        owner.jobs.unreported().len(),
        1,
        "the job is still owed an announcement"
    );
    owner.root.cancel();
}
