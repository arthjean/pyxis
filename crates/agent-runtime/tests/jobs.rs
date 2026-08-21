//! The registry seen from the thread that owns it (EP-041).
//!
//! Everything below goes through [`ThreadHandle::start`]: the registry is bound
//! to a real actor, its writes go through the real mailbox, and its teardown is
//! the real shutdown. What is asserted is the DURABLE log, because that is what
//! a reopened thread reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::event::AgentEvent;
use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::id::{JobId, SequentialIds, ThreadId, TurnId};
use agent_runtime::jobs::{
    JobError, JobKind, JobOutcome, JobRegistry, JobStatus, MAX_ACTIVE_JOBS, TEARDOWN_CAUSE,
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
async fn open(store: Arc<dyn ThreadStore>, seed: u64) -> Owner {
    let ids = Arc::new(SequentialIds::starting_at(seed));
    let thread_id = store
        .read()
        .await
        .expect("the store reads")
        .thread_id
        .unwrap_or_else(|| ThreadId::generate(ids.as_ref()));
    let jobs = JobRegistry::new(Arc::clone(&ids) as Arc<_>, Arc::new(FrozenClock));
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store: Arc::clone(&store),
        runner: Arc::new(IdleRunner),
        turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
            ids.as_ref(),
        )))) as Arc<dyn TurnContextSource>,
        ids,
        clock: Arc::new(FrozenClock),
        parent_cancel: root.clone(),
        agents: None,
        jobs: Some(Arc::clone(&jobs)),
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
        .register(JobKind::Terminal, "npm test")
        .await
        .expect("the registration is accepted");
    let stopped = first
        .jobs
        .register(JobKind::Terminal, "npm run dev")
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

/// US-133 AC5: the terminal transition is in the log before the caller that
/// would announce it holds anything to announce.
#[tokio::test]
async fn a_terminal_reaches_the_log_before_the_settle_answers() {
    let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
    let owner = open(Arc::clone(&store), 1).await;
    let job = owner
        .jobs
        .register(JobKind::Terminal, "npm test")
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
        .register(JobKind::Terminal, "npm run dev")
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
        .register(JobKind::Terminal, "npm run dev")
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
                .register(JobKind::Terminal, format!("tail -f {index}.log"))
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
            tokio::spawn(async move { jobs.register(JobKind::Terminal, "npm run dev").await });
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
