//! Bounded registry of the background jobs of one thread (EP-041).
//!
//! A background job is a process a turn started and did NOT wait for. Its
//! output arrives later, its exit arrives later still, and by then the turn that
//! started it may be over. The registry is the accounting side of that promise:
//! it reserves a slot, mints an identifier and makes the registration durable
//! BEFORE anything is launched, so a job the log does not carry is a job nobody
//! can account for after a restart.
//!
//! What it deliberately does not do is launch anything. Spawning the process,
//! draining its output and settling it when it exits belong to the crates that
//! own a terminal; the registry only knows that a job exists, which state it
//! reached, and whether its owner was told (EP-042 plugs a launcher into it).
//!
//! Three properties are load-bearing:
//! - the bound is a CONSTANT ([`MAX_ACTIVE_JOBS`]), never a configuration key
//!   (FR-20). A full registry refuses immediately; it never waits.
//! - a job belongs to ONE thread. The registry is per-thread, so a job of
//!   another thread is not merely refused, it is invisible.
//! - the FIRST arrival settles a job. A process that exits while a cancellation
//!   is in flight wins over the cancellation, and the loser writes nothing.
//!
//! Adding a variant to [`JobStatus`] is a compile error here, not a silent
//! default: [`JobStatus::is_active`], [`JobStatus::is_terminal`] and
//! [`JobStatus::as_str`] all match every variant explicitly and carry no
//! catch-all arm.

use std::sync::{Arc, Mutex, OnceLock};

use agent_core::clock::Clock;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::event::{ThreadEventPayload, ThreadJournal};
use crate::id::{IdGenerator, JobId, ThreadId};

/// Background jobs a thread may run AT ONCE.
///
/// Deliberately the same value as `agent_tools::exec_session::MAX_SESSIONS`: a
/// background job holds a live process and its pipes exactly like an open shell
/// session does, and letting a model hold two different budgets of long-lived
/// processes would make the total it can pin unbounded in practice. The two
/// constants stay independent because the two crates cannot depend on each
/// other, but they are meant to move together.
pub const MAX_ACTIVE_JOBS: usize = 4;

/// Characters a registered command line may carry in the durable log. A command
/// is an invocation, not a script.
pub const MAX_JOB_COMMAND: usize = 4_000;

/// Cause persisted for a job its thread stopped on the way down. Named so every
/// surface reports the same sentence for the same event.
pub const TEARDOWN_CAUSE: &str = "interrupted: the thread stopped while the job was running";

/// What kind of thing runs in the background.
///
/// Exactly one genre in v1. The harness this design comes from also runs
/// sub-agents in the background, but sub-agents already have an owner here:
/// [`crate::supervisor::AgentSupervisor`] holds their slots, their filiation and
/// their handoffs, and ADR-13 keeps them out of the MVP anyway. Registering
/// them a second time would give one child two budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A process attached to a terminal, started by a turn and not waited for.
    Terminal,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
        }
    }
}

/// Lifecycle of a background job.
///
/// `Stopping` is what makes a stop observable: between "a stop was requested"
/// and "the process is gone" there is a window in which the process is still
/// alive and still writing, and collapsing the two would let a second stop
/// believe it had something left to do while the first one was still unwinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// The process runs. Occupies one of the [`MAX_ACTIVE_JOBS`] slots.
    Running,
    /// A stop was REQUESTED and the process has not answered yet. Still holds
    /// its slot: the process is alive until it is not.
    Stopping,
    /// Terminal: the process exited on its own.
    Completed,
    /// Terminal: the process was stopped by its owner, a shutdown or a resume.
    Killed,
    /// Terminal: the process could not run, or its supervision failed.
    Failed,
}

impl JobStatus {
    /// Does this state hold one of the concurrency slots?
    pub fn is_active(self) -> bool {
        match self {
            Self::Running | Self::Stopping => true,
            Self::Completed | Self::Killed | Self::Failed => false,
        }
    }

    /// Is this state final? A job settles exactly once.
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Completed | Self::Killed | Self::Failed => true,
            Self::Running | Self::Stopping => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }
}

/// How a job ended, with what the caller has to say about it.
///
/// Carries the exit code at the moment it is known, so the terminal transition
/// is durable in ONE event: reconstructing an exit code from a later line would
/// mean a log where a job is finished and its result is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// The process exited on its own, with this status code.
    Completed { exit_code: i32 },
    /// The process was stopped.
    Killed { cause: String },
    /// The process never ran, or its supervision broke.
    Failed { cause: String },
}

impl JobOutcome {
    /// Terminal state this outcome settles a job in.
    pub fn status(&self) -> JobStatus {
        match self {
            Self::Completed { .. } => JobStatus::Completed,
            Self::Killed { .. } => JobStatus::Killed,
            Self::Failed { .. } => JobStatus::Failed,
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Completed { exit_code } => Some(*exit_code),
            Self::Killed { .. } | Self::Failed { .. } => None,
        }
    }

    fn cause(&self) -> Option<String> {
        match self {
            Self::Completed { .. } => None,
            Self::Killed { cause } | Self::Failed { cause } => Some(cause.clone()),
        }
    }
}

/// One background job of a thread, as the log describes it.
///
/// `reported` is a field of its own and NOT a sixth status. The invariant is
/// one-way: a finished job may still be unreported, and a reported job is
/// necessarily finished. Announcing a result is what a thread owes its model,
/// and it happens once; the two would have to be re-derived from each other at
/// every resume if they shared a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: JobId,
    pub kind: JobKind,
    /// Command line the job was registered for, bounded at registration.
    pub command: String,
    pub status: JobStatus,
    /// Has the terminal state been announced to the owner of the thread?
    pub reported: bool,
    /// Exit status, for a job that exited on its own.
    pub exit_code: Option<i32>,
    /// Bounded cause of a non-nominal end.
    pub cause: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}

/// What a caller reads about a job.
///
/// Returned BY VALUE, always. No method of the registry lends a reference to
/// live state: a reader holding one would see a job settle under it, and the
/// lock it would have to hold to prevent that is the lock every settle needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub job_id: JobId,
    pub kind: JobKind,
    pub command: String,
    pub status: JobStatus,
    pub reported: bool,
    pub exit_code: Option<i32>,
    pub cause: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    /// `true` while THIS process holds the job's cancellation node. A job
    /// rebuilt from a log is detached: nothing here can stop it, because the
    /// process that owned it is gone.
    pub attached: bool,
}

/// Identity handed back for an accepted registration.
///
/// The token is the caller's half of the contract: a launcher hangs its process
/// from it and never keeps a `JoinHandle` to abort, because cancellation
/// descends the tree and an abort would cut a job between its exit and the
/// settle that makes it durable.
#[derive(Debug, Clone)]
pub struct RegisteredJob {
    pub job_id: JobId,
    pub cancel: CancellationToken,
}

/// Why a job operation was refused. Every variant refuses BEFORE any process is
/// started, and none of them reveals anything about a thread the caller does not
/// own.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    #[error(
        "background job limit reached ({active}/{MAX_ACTIVE_JOBS} running): stop one (kill its job id) before starting another"
    )]
    LimitReached { active: usize },
    /// Unknown, or owned by another thread: ONE error for both, so a probe
    /// cannot tell them apart.
    #[error("background job {job_id} is not reachable from this thread")]
    Unknown { job_id: JobId },
    #[error("background jobs are not available: no registry is bound to this thread")]
    Detached,
    /// The registration could not be made durable. Nothing was launched.
    #[error("background job was not registered: {0}")]
    NotRegistered(String),
}

/// Thread link, bound once the thread actor exists.
struct JobOwner {
    journal: Arc<dyn ThreadJournal>,
    /// Cancellation node the jobs hang from. A child of the THREAD's token:
    /// cancelling the thread reaches every job, and cancelling one job reaches
    /// neither its thread nor a sibling (FR-10, invariant 13).
    cancel: CancellationToken,
}

struct Job {
    record: JobRecord,
    /// `None` for a job rebuilt from a log: its process belongs to a run that
    /// no longer exists.
    cancel: Option<CancellationToken>,
}

impl Job {
    fn snapshot(&self) -> JobSnapshot {
        let record = &self.record;
        JobSnapshot {
            job_id: record.job_id,
            kind: record.kind,
            command: record.command.clone(),
            status: record.status,
            reported: record.reported,
            exit_code: record.exit_code,
            cause: record.cause.clone(),
            started_at_ms: record.started_at_ms,
            ended_at_ms: record.ended_at_ms,
            attached: self.cancel.is_some(),
        }
    }
}

#[derive(Default)]
struct RegistryState {
    /// Slots taken by a registration that is being made durable. Counted with
    /// the running jobs, so the last slot cannot be sold twice while the log
    /// write of the first buyer is still in flight.
    reserved: usize,
    jobs: Vec<Job>,
}

impl RegistryState {
    fn active(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.record.status.is_active())
            .count()
    }
}

/// Owner of the background jobs of ONE thread.
///
/// Interior mutability on purpose: the tools that start a job run inside a turn
/// task, not inside the thread actor, so a reservation has to be atomic on its
/// own rather than by virtue of who calls it.
pub struct JobRegistry {
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    owner: OnceLock<JobOwner>,
    state: Mutex<RegistryState>,
    /// Transitions the log refused while the thread was closing. Handed back at
    /// shutdown so the actor, which is still the writer, persists them itself
    /// instead of leaving the registry to be repaired at the next resume.
    unrecorded: Mutex<Vec<ThreadEventPayload>>,
}

impl JobRegistry {
    pub fn new(ids: Arc<dyn IdGenerator>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            ids,
            clock,
            owner: OnceLock::new(),
            state: Mutex::new(RegistryState::default()),
            unrecorded: Mutex::new(Vec::new()),
        })
    }

    /// Binds the registry to its thread. Called once by
    /// [`crate::thread::ThreadHandle::start`]; before it, every registration is
    /// refused as detached.
    pub(crate) fn attach(
        &self,
        thread_id: ThreadId,
        journal: Arc<dyn ThreadJournal>,
        cancel: CancellationToken,
    ) {
        if self.owner.set(JobOwner { journal, cancel }).is_err() {
            // A registry belongs to ONE thread: its slots and its jobs are that
            // thread's. Rebinding would make a second thread spend the first
            // one's budget, and would hang its processes from a foreign
            // cancellation node.
            tracing::warn!(
                thread_id = %thread_id,
                "background job registry already bound to another thread; the binding is kept"
            );
        }
    }

    /// A poisoned lock is recovered rather than propagated: the accounting is
    /// plain data, and losing the whole registry is worse than reading a map a
    /// panicking thread left consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn owner(&self) -> Result<&JobOwner, JobError> {
        self.owner.get().ok_or(JobError::Detached)
    }

    /// Every job of this thread, in registration order.
    pub fn snapshots(&self) -> Vec<JobSnapshot> {
        self.lock().jobs.iter().map(Job::snapshot).collect()
    }

    /// One job of THIS thread. A job of another thread is not in this registry,
    /// so it reads as absent rather than as forbidden.
    pub fn get(&self, job_id: JobId) -> Option<JobSnapshot> {
        self.lock()
            .jobs
            .iter()
            .find(|job| job.record.job_id == job_id)
            .map(Job::snapshot)
    }

    /// Jobs currently holding a slot.
    pub fn active(&self) -> usize {
        self.lock().active()
    }

    /// Registers a background job and hands back its identity.
    ///
    /// Order is the contract: take the slot, mint the identifier, make the
    /// registration durable, and only then admit the job. A caller gets the
    /// identifier once the log carries it, so nothing is ever launched for a
    /// job a restart could not find (US-133, invariant 12).
    ///
    /// The slot is taken BEFORE the durable write and given back if that write
    /// fails: a refusal must not leak the budget it never spent.
    pub async fn register(
        &self,
        kind: JobKind,
        command: impl Into<String>,
    ) -> Result<RegisteredJob, JobError> {
        let owner = self.owner()?;
        let command = bounded_command(command.into());
        self.reserve()?;
        let job_id = JobId::generate(self.ids.as_ref());
        if let Err(err) = owner
            .journal
            .record(ThreadEventPayload::JobRegistered {
                job_id,
                job_kind: kind,
                command: command.clone(),
            })
            .await
        {
            self.release();
            return Err(JobError::NotRegistered(err.to_string()));
        }

        let cancel = owner.cancel.child_token();
        {
            let mut state = self.lock();
            state.reserved = state.reserved.saturating_sub(1);
            state.jobs.push(Job {
                record: JobRecord {
                    job_id,
                    kind,
                    command,
                    status: JobStatus::Running,
                    reported: false,
                    exit_code: None,
                    cause: None,
                    started_at_ms: self.clock.now_ms(),
                    ended_at_ms: None,
                },
                cancel: Some(cancel.clone()),
            });
        }

        // A thread that was interrupted DURING this registration leaves the
        // node already cancelled. Settling the job here, before anyone can act
        // on the identifier, is what keeps the registry from ever holding a
        // record whose process nobody will start (US-134 AC6).
        if cancel.is_cancelled() {
            self.cancel(job_id, TEARDOWN_CAUSE).await?;
        }
        Ok(RegisteredJob { job_id, cancel })
    }

    /// Settles a job in its terminal state.
    ///
    /// The FIRST arrival wins: the terminal is claimed in memory under the
    /// lock, and a second settle finds the job already terminal, traces it and
    /// writes nothing. Claiming is not publishing, so the transition is durable
    /// before the snapshot that announces it exists (invariant 11).
    ///
    /// `Ok(None)` means the job was already settled by someone else.
    pub async fn settle(
        &self,
        job_id: JobId,
        outcome: JobOutcome,
    ) -> Result<Option<JobSnapshot>, JobError> {
        self.owner()?;
        let now = self.clock.now_ms();
        let snapshot = {
            let mut state = self.lock();
            let Some(job) = state
                .jobs
                .iter_mut()
                .find(|job| job.record.job_id == job_id)
            else {
                return Err(JobError::Unknown { job_id });
            };
            if job.record.status.is_terminal() {
                tracing::debug!(
                    job_id = %job_id,
                    status = job.record.status.as_str(),
                    "background job already settled; the second outcome is ignored"
                );
                return Ok(None);
            }
            job.record.status = outcome.status();
            job.record.exit_code = outcome.exit_code();
            job.record.cause = outcome.cause();
            job.record.ended_at_ms = Some(now);
            job.snapshot()
        };

        self.journal(ThreadEventPayload::JobStateChanged {
            job_id,
            to: snapshot.status,
            exit_code: snapshot.exit_code,
            cause: snapshot.cause.clone(),
        })
        .await;
        Ok(Some(snapshot))
    }

    /// Stops one job and settles it, whatever the cancellation achieves.
    ///
    /// The report flag is raised and made durable BEFORE the stop is attempted:
    /// a job its thread is tearing down has no reader left, and a flag written
    /// after a cancellation that hangs would be a flag nobody writes at all. The
    /// job then settles even if the process ignores its token, so the registry
    /// never keeps a record waiting for an answer that will not come
    /// (US-134 AC3).
    ///
    /// Siblings are untouched: each job hangs from its own node.
    pub async fn cancel(&self, job_id: JobId, cause: &str) -> Result<JobSnapshot, JobError> {
        self.owner()?;
        let (payloads, token) = {
            let mut state = self.lock();
            let Some(job) = state
                .jobs
                .iter_mut()
                .find(|job| job.record.job_id == job_id)
            else {
                return Err(JobError::Unknown { job_id });
            };
            if job.record.status.is_terminal() {
                return Ok(job.snapshot());
            }
            let mut payloads = Vec::new();
            if !job.record.reported {
                job.record.reported = true;
                payloads.push(ThreadEventPayload::JobReported { job_id });
            }
            if job.record.status == JobStatus::Running {
                job.record.status = JobStatus::Stopping;
                payloads.push(ThreadEventPayload::JobStateChanged {
                    job_id,
                    to: JobStatus::Stopping,
                    exit_code: None,
                    cause: None,
                });
            }
            (payloads, job.cancel.clone())
        };
        for payload in payloads {
            self.journal(payload).await;
        }

        match token {
            Some(token) => token.cancel(),
            None => tracing::debug!(
                job_id = %job_id,
                "background job holds no cancellation node in this process; settling it anyway"
            ),
        }

        match self
            .settle(
                job_id,
                JobOutcome::Killed {
                    cause: cause.to_string(),
                },
            )
            .await?
        {
            Some(snapshot) => Ok(snapshot),
            // The process exited while the stop was in flight. First arrived
            // wins, so its own outcome is the one the log and the reader keep.
            None => self.get(job_id).ok_or(JobError::Unknown { job_id }),
        }
    }

    /// Stops every active job of the thread, in registration order.
    ///
    /// Called by the thread actor on its way down, inside the shutdown budget it
    /// already has: a job holds a process, and no process may outlive the thread
    /// that started it (US-134 AC2).
    pub(crate) async fn cancel_all(&self) {
        let job_ids: Vec<JobId> = self
            .lock()
            .jobs
            .iter()
            .filter(|job| job.record.status.is_active())
            .map(|job| job.record.job_id)
            .collect();
        for job_id in job_ids {
            if let Err(err) = self.cancel(job_id, TEARDOWN_CAUSE).await {
                tracing::warn!(job_id = %job_id, error = %err, "background job not stopped at shutdown");
            }
        }
    }

    /// Rebuilds the registry from a resumed log (US-133).
    ///
    /// The records are kept EXACTLY as the log describes them, terminal state
    /// and report flag included, and no process is started: reopening a thread
    /// reads history, it does not replay it. A job the log left active is
    /// detached, so nothing here pretends it can still be stopped.
    pub(crate) fn restore(&self, records: Vec<JobRecord>) {
        let mut state = self.lock();
        state.reserved = 0;
        state.jobs = records
            .into_iter()
            .map(|record| Job {
                record,
                cancel: None,
            })
            .collect();
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

    /// Takes a slot for a registration in flight, or refuses immediately.
    ///
    /// Never waits: a model that hits the bound gets an answer naming what to
    /// release, which it can act on, instead of a call that returns when some
    /// other job happens to end.
    fn reserve(&self) -> Result<(), JobError> {
        let mut state = self.lock();
        let active = state.active() + state.reserved;
        if active >= MAX_ACTIVE_JOBS {
            return Err(JobError::LimitReached { active });
        }
        state.reserved += 1;
        Ok(())
    }

    fn release(&self) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(1);
    }

    /// Persists a transition, or keeps it for the actor's own shutdown.
    ///
    /// A refusal here is never a lost transition: the memory state already
    /// carries it, and the payload is handed back through
    /// [`Self::take_unrecorded`] to the one writer that still has the log open.
    async fn journal(&self, payload: ThreadEventPayload) {
        let Ok(owner) = self.owner() else { return };
        if let Err(err) = owner.journal.record(payload.clone()).await {
            tracing::debug!(error = %err, "background job transition deferred to the thread actor");
            self.unrecorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload);
        }
    }
}

fn bounded_command(command: String) -> String {
    match command.char_indices().nth(MAX_JOB_COMMAND) {
        None => command,
        Some((cut, _)) => command[..cut].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::id::{EventId, SequentialIds};
    use crate::thread::SubmitError;

    /// Durable sink of a thread that is not there: records what was written and
    /// can be made to refuse, which is the only way to reach the deferred path
    /// without a real actor.
    struct Log {
        ids: SequentialIds,
        written: Mutex<Vec<ThreadEventPayload>>,
        refuses: AtomicBool,
    }

    impl Log {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                ids: SequentialIds::new(),
                written: Mutex::new(Vec::new()),
                refuses: AtomicBool::new(false),
            })
        }

        fn written(&self) -> Vec<ThreadEventPayload> {
            self.written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl ThreadJournal for Log {
        async fn record(&self, payload: ThreadEventPayload) -> Result<EventId, SubmitError> {
            if self.refuses.load(Ordering::SeqCst) {
                return Err(SubmitError::Stopped);
            }
            self.written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload);
            Ok(EventId::generate(&self.ids))
        }
    }

    struct FrozenClock;

    #[async_trait::async_trait]
    impl Clock for FrozenClock {
        fn now_ms(&self) -> u64 {
            1_700_000_000_000
        }
        async fn sleep(&self, _dur: std::time::Duration) {}
    }

    fn bound(log: &Arc<Log>) -> (Arc<JobRegistry>, CancellationToken) {
        let registry = JobRegistry::new(Arc::new(SequentialIds::new()), Arc::new(FrozenClock));
        let thread = CancellationToken::new();
        registry.attach(
            ThreadId::generate(&SequentialIds::new()),
            Arc::clone(log) as Arc<dyn ThreadJournal>,
            thread.child_token(),
        );
        (registry, thread)
    }

    #[tokio::test]
    async fn a_registration_is_durable_before_the_identifier_is_handed_back() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);

        let job = registry
            .register(JobKind::Terminal, "npm run dev")
            .await
            .expect("the registration is accepted");

        assert!(
            matches!(
                log.written().first(),
                Some(ThreadEventPayload::JobRegistered { job_id, .. }) if *job_id == job.job_id
            ),
            "the log carries the registration before the caller learns the identifier"
        );
        let snapshot = registry.get(job.job_id).expect("the job is registered");
        assert_eq!(snapshot.status, JobStatus::Running);
        assert!(!snapshot.reported);
        assert!(snapshot.attached, "the job hangs from a live node");
    }

    #[tokio::test]
    async fn a_detached_registry_refuses_before_anything_is_written() {
        let registry = JobRegistry::new(Arc::new(SequentialIds::new()), Arc::new(FrozenClock));
        assert_eq!(
            registry.register(JobKind::Terminal, "sleep 1").await.err(),
            Some(JobError::Detached)
        );
        assert!(registry.snapshots().is_empty());
    }

    /// US-132 AC2: the bound refuses, it never queues. The message names both
    /// the limit and what to do about it.
    #[tokio::test]
    async fn a_full_registry_refuses_immediately_naming_the_bound_and_the_way_out() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        for _ in 0..MAX_ACTIVE_JOBS {
            registry
                .register(JobKind::Terminal, "tail -f log")
                .await
                .expect("a slot is free");
        }

        let refused = registry
            .register(JobKind::Terminal, "one too many")
            .await
            .expect_err("the registry is full");

        assert_eq!(
            refused,
            JobError::LimitReached {
                active: MAX_ACTIVE_JOBS
            }
        );
        let message = refused.to_string();
        assert!(message.contains(&MAX_ACTIVE_JOBS.to_string()));
        assert!(message.contains("stop one"));
        assert_eq!(registry.active(), MAX_ACTIVE_JOBS);
        assert_eq!(
            log.written().len(),
            MAX_ACTIVE_JOBS,
            "a refusal writes nothing"
        );
    }

    /// A settled job gives its slot back, and only then.
    #[tokio::test]
    async fn a_settled_job_frees_the_slot_it_held() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let mut jobs = Vec::new();
        for _ in 0..MAX_ACTIVE_JOBS {
            jobs.push(
                registry
                    .register(JobKind::Terminal, "tail -f log")
                    .await
                    .expect("a slot is free"),
            );
        }
        registry
            .settle(jobs[0].job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");

        assert_eq!(registry.active(), MAX_ACTIVE_JOBS - 1);
        registry
            .register(JobKind::Terminal, "the released slot")
            .await
            .expect("the freed slot is usable");
    }

    /// US-132 AC5: the slot is taken BEFORE the durable write, and given back
    /// when that write fails. Nothing is registered and nothing is launched.
    #[tokio::test]
    async fn a_registration_whose_log_refuses_gives_its_slot_back_and_registers_nothing() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        log.refuses.store(true, Ordering::SeqCst);

        let refused = registry
            .register(JobKind::Terminal, "npm run dev")
            .await
            .expect_err("a registration the log refuses is refused");

        assert!(matches!(refused, JobError::NotRegistered(_)));
        assert!(registry.snapshots().is_empty());
        assert_eq!(registry.active(), 0);
        log.refuses.store(false, Ordering::SeqCst);
        registry
            .register(JobKind::Terminal, "npm run dev")
            .await
            .expect("the slot was given back");
    }

    /// US-132 AC4: two settles for one job. The first wins, the second changes
    /// nothing and writes nothing.
    #[tokio::test]
    async fn the_first_settle_wins_and_the_second_is_ignored() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let job = registry
            .register(JobKind::Terminal, "npm test")
            .await
            .expect("the registration is accepted");

        let first = registry
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known")
            .expect("the first settle wins");
        let second = registry
            .settle(
                job.job_id,
                JobOutcome::Failed {
                    cause: "too late".into(),
                },
            )
            .await
            .expect("the job is known");

        assert_eq!(first.status, JobStatus::Completed);
        assert_eq!(first.exit_code, Some(0));
        assert!(second.is_none(), "the second settle is ignored");
        assert_eq!(
            registry.get(job.job_id).map(|snapshot| snapshot.status),
            Some(JobStatus::Completed)
        );
        let terminals = log
            .written()
            .into_iter()
            .filter(|payload| matches!(payload, ThreadEventPayload::JobStateChanged { .. }))
            .count();
        assert_eq!(terminals, 1, "one terminal per job, in the log too");
    }

    /// US-132 AC4: a settle for an identifier this thread never registered is a
    /// named refusal, not a silent insertion.
    #[tokio::test]
    async fn a_settle_on_an_unknown_job_refuses_and_inserts_nothing() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let stranger = JobId::generate(&SequentialIds::starting_at(900));

        assert_eq!(
            registry
                .settle(stranger, JobOutcome::Completed { exit_code: 0 })
                .await
                .err(),
            Some(JobError::Unknown { job_id: stranger })
        );
        assert!(registry.snapshots().is_empty());
        assert!(log.written().is_empty());
    }

    /// US-134 AC3: the report flag is durable BEFORE the stop is attempted, and
    /// the job settles even though nothing here can observe the process.
    #[tokio::test]
    async fn a_stop_reports_the_job_before_it_cancels_and_settles_it_anyway() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let job = registry
            .register(JobKind::Terminal, "tail -f log")
            .await
            .expect("the registration is accepted");

        let stopped = registry
            .cancel(job.job_id, TEARDOWN_CAUSE)
            .await
            .expect("the job is known");

        assert_eq!(stopped.status, JobStatus::Killed);
        assert!(stopped.reported);
        assert!(job.cancel.is_cancelled(), "the job's node was cancelled");
        let order: Vec<&'static str> = log
            .written()
            .iter()
            .map(|payload| match payload {
                ThreadEventPayload::JobRegistered { .. } => "registered",
                ThreadEventPayload::JobReported { .. } => "reported",
                ThreadEventPayload::JobStateChanged { to, .. } => to.as_str(),
                _ => "other",
            })
            .collect();
        assert_eq!(
            order,
            vec!["registered", "reported", "stopping", "killed"],
            "the flag is written before the stop, and both states are durable"
        );
    }

    /// US-134 AC4: cancellation descends one branch. A sibling keeps running and
    /// keeps its node.
    #[tokio::test]
    async fn stopping_one_job_leaves_its_siblings_untouched() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let stopped = registry
            .register(JobKind::Terminal, "tail -f a")
            .await
            .expect("the registration is accepted");
        let sibling = registry
            .register(JobKind::Terminal, "tail -f b")
            .await
            .expect("the registration is accepted");

        registry
            .cancel(stopped.job_id, TEARDOWN_CAUSE)
            .await
            .expect("the job is known");

        assert!(!sibling.cancel.is_cancelled());
        let sibling = registry.get(sibling.job_id).expect("the sibling is known");
        assert_eq!(sibling.status, JobStatus::Running);
        assert!(!sibling.reported);
    }

    /// Edge case #3: the process exits while the stop is in flight. First
    /// arrived wins, so the log and the reader keep the real exit.
    #[tokio::test]
    async fn a_process_that_exits_during_a_stop_keeps_its_own_outcome() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let job = registry
            .register(JobKind::Terminal, "npm test")
            .await
            .expect("the registration is accepted");
        registry
            .cancel(job.job_id, TEARDOWN_CAUSE)
            .await
            .expect("the job is known");

        let late = registry
            .settle(job.job_id, JobOutcome::Completed { exit_code: 0 })
            .await
            .expect("the job is known");

        assert!(late.is_none(), "the exit lost the race and writes nothing");
        assert_eq!(
            registry.get(job.job_id).map(|snapshot| snapshot.status),
            Some(JobStatus::Killed)
        );
    }

    /// US-131 AC6: the outward view survives serialization, and it is a value:
    /// mutating the registry afterwards does not touch a snapshot already read.
    #[tokio::test]
    async fn a_snapshot_round_trips_and_does_not_follow_the_registry() {
        let log = Log::new();
        let (registry, _thread) = bound(&log);
        let job = registry
            .register(JobKind::Terminal, "npm run dev")
            .await
            .expect("the registration is accepted");
        let taken = registry.get(job.job_id).expect("the job is known");

        registry
            .settle(job.job_id, JobOutcome::Completed { exit_code: 2 })
            .await
            .expect("the job is known");

        assert_eq!(taken.status, JobStatus::Running, "the copy did not move");
        let line = serde_json::to_string(&taken).expect("a snapshot serializes");
        assert_eq!(
            serde_json::from_str::<JobSnapshot>(&line).expect("a snapshot deserializes"),
            taken
        );
    }

    /// US-133: a registry rebuilt from a log keeps what the log says, launches
    /// nothing, and admits it can no longer stop anything.
    #[test]
    fn a_restored_job_keeps_its_state_and_its_flag_and_is_detached() {
        let registry = JobRegistry::new(Arc::new(SequentialIds::new()), Arc::new(FrozenClock));
        let job_id = JobId::generate(&SequentialIds::starting_at(70));
        registry.restore(vec![JobRecord {
            job_id,
            kind: JobKind::Terminal,
            command: "npm run dev".into(),
            status: JobStatus::Completed,
            reported: true,
            exit_code: Some(0),
            cause: None,
            started_at_ms: 1,
            ended_at_ms: Some(2),
        }]);

        let snapshot = registry.get(job_id).expect("the job came back");
        assert_eq!(snapshot.status, JobStatus::Completed);
        assert!(snapshot.reported);
        assert_eq!(snapshot.exit_code, Some(0));
        assert!(!snapshot.attached, "no process was restarted");
        assert_eq!(registry.active(), 0);
    }

    /// A command longer than the bound is cut at a character boundary, so a
    /// durable line never carries an unbounded model-written string.
    #[test]
    fn a_command_is_bounded_before_it_reaches_the_log() {
        let long = "e\u{301}".repeat(MAX_JOB_COMMAND);
        let bounded = bounded_command(long);
        assert_eq!(bounded.chars().count(), MAX_JOB_COMMAND);
    }
}
