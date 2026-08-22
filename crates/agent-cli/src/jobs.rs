//! Where a recorded background job becomes a running process (EP-042, US-135).
//!
//! `agent-runtime` owns the accounting: the slot, the identifier, the durable
//! record, the terminal state. It deliberately does not know what a shell is.
//! `agent-tools` owns the terminal but holds no thread, so it cannot own the
//! registry either. The binary is the only crate that sees both, and this is
//! where the two are joined, on the same pattern `SubAgentSpawner` follows for
//! sub-agents.
//!
//! The registry hands the launcher a `token`, which for a terminal is the
//! session id `ExecSessions::reserve` already minted. Reading it is this
//! module's business, never the registry's.

use std::sync::Arc;

use agent_runtime::jobs::{JobLaunch, JobLauncher, JobProcess};
use agent_tools::ExecSessions;

/// Starts the terminal a registered job stands for.
pub struct TerminalJobLauncher {
    sessions: ExecSessions,
}

impl TerminalJobLauncher {
    pub fn new(sessions: ExecSessions) -> Self {
        Self { sessions }
    }
}

#[async_trait::async_trait]
impl JobLauncher for TerminalJobLauncher {
    async fn launch(&self, launch: JobLaunch) -> Result<Arc<dyn JobProcess>, String> {
        self.sessions
            .launch_staged(launch.token, Some(launch.job_id))
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(TerminalJobProcess {
            sessions: self.sessions.clone(),
            session_id: launch.token,
        }) as Arc<dyn JobProcess>)
    }
}

/// The two things the registry cannot do to a terminal by itself.
struct TerminalJobProcess {
    sessions: ExecSessions,
    session_id: u64,
}

#[async_trait::async_trait]
impl JobProcess for TerminalJobProcess {
    async fn read_output(&self) -> Result<Vec<u8>, String> {
        self.sessions
            .drain_output(self.session_id)
            .map_err(|e| e.to_string())
    }

    async fn stop(&self) {
        if let Err(err) = self.sessions.terminate(self.session_id).await {
            tracing::debug!(
                session = self.session_id,
                error = %err,
                "background terminal was not stopped cleanly"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use agent_core::clock::SystemClock;
    use agent_core::event::AgentEvent;
    use agent_runtime::context::{FixedTurnContext, TurnContext, TurnContextSource, TurnLimits};
    use agent_runtime::event::ThreadEventPayload;
    use agent_runtime::id::{SequentialIds, ThreadId, TurnId};
    use agent_runtime::jobs::{JobRegistry, JobStatus};
    use agent_runtime::runner::{TurnOutcome, TurnRequest, TurnRunner};
    use agent_runtime::store::{MemoryThreadStore, ThreadStore};
    use agent_runtime::thread::{ThreadHandle, ThreadOptions};
    use agent_tools::{Tool, ToolCtx};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// No turn ever runs here: a terminal is opened by a tool call, and every
    /// assertion below reads the registry and the durable log, not a model.
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

    /// A thread that owns a registry wired to the REAL terminal launcher, plus
    /// the `ToolCtx` the two terminal tools are called through. This is the
    /// production path: `exec_command` reaches the registry, the registry
    /// reaches [`TerminalJobLauncher`], and the launcher spawns.
    struct Wired {
        _handle: ThreadHandle,
        store: Arc<dyn ThreadStore>,
        registry: Arc<JobRegistry>,
        ctx: ToolCtx,
    }

    async fn wired() -> Wired {
        // The tools only need a directory that exists; nothing below writes to
        // it, so the process temp directory is enough and leaves nothing.
        let dir = std::env::temp_dir();
        let sessions = ExecSessions::new();
        let ids = Arc::new(SequentialIds::starting_at(11));
        let store = Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>;
        let registry = JobRegistry::new(
            Arc::clone(&ids) as Arc<_>,
            Arc::new(SystemClock),
            Some(Arc::new(TerminalJobLauncher::new(sessions.clone()))),
        );
        sessions.job_handle().bind(Arc::clone(&registry));
        let turn_context = TurnContext {
            turn_id: TurnId::generate(ids.as_ref()),
            model: "gpt-5.4-codex".into(),
            reasoning_effort: None,
            model_runtime_fingerprint: None,
            permission_mode: "ask".into(),
            sandbox: "workspace-write".into(),
            workspace: dir.clone(),
            limits: TurnLimits {
                max_output_tokens: 1024,
                max_pending_inputs: 16,
            },
        };
        let handle = ThreadHandle::start(ThreadOptions {
            thread_id: ThreadId::generate(ids.as_ref()),
            store: Arc::clone(&store),
            runner: Arc::new(IdleRunner),
            turn_contexts: Arc::new(FixedTurnContext::new(turn_context))
                as Arc<dyn TurnContextSource>,
            ids,
            clock: Arc::new(SystemClock),
            parent_cancel: CancellationToken::new(),
            agents: None,
            jobs: Some(Arc::clone(&registry)),
        })
        .await
        .expect("the thread starts");
        let mut ctx = ToolCtx::new(dir);
        ctx.sessions = sessions;
        Wired {
            _handle: handle,
            store,
            registry,
            ctx,
        }
    }

    fn exec(cmd: &str, yield_ms: u64) -> agent_tools::exec_session::ExecCommandInput {
        agent_tools::exec_session::ExecCommandInput {
            cmd: cmd.to_string(),
            workdir: None,
            shell: None,
            tty: None,
            yield_time_ms: Some(yield_ms),
            max_output_tokens: None,
        }
    }

    /// Everything the durable log says about jobs, in order.
    async fn job_trace(store: &Arc<dyn ThreadStore>) -> Vec<String> {
        store
            .read()
            .await
            .expect("the store reads")
            .events
            .iter()
            .filter_map(|event| match &event.payload {
                ThreadEventPayload::JobRegistered { command, .. } => {
                    Some(format!("registered {command}"))
                }
                ThreadEventPayload::JobStateChanged {
                    to,
                    exit_code,
                    cause,
                    ..
                } => Some(format!(
                    "{} {:?} {:?}",
                    to.as_str(),
                    exit_code,
                    cause.as_deref()
                )),
                ThreadEventPayload::JobReported { .. } => Some("reported".to_string()),
                _ => None,
            })
            .collect()
    }

    /// The single settle of a job, or a readable failure.
    fn only_job(registry: &JobRegistry) -> agent_runtime::JobSnapshot {
        let mut snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1, "exactly one job was registered");
        snapshots.remove(0)
    }

    /// US-136 AC1/AC2: a terminal that stays open is a job of the thread, and
    /// the log carries it. `open_sessions` reads the registry to say so.
    #[tokio::test]
    async fn a_terminal_is_registered_with_the_thread_and_listed_through_the_registry() {
        let w = wired().await;

        let out = agent_tools::ExecCommand
            .call(exec("sleep 30", 250), &w.ctx)
            .await
            .expect("the session opens");

        let job = only_job(&w.registry);
        assert_eq!(job.command, "sleep 30");
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(
            job_trace(&w.store).await,
            vec!["registered sleep 30".to_string()],
            "the log carries the registration and nothing else yet"
        );
        assert_eq!(
            w.ctx.sessions.open_sessions(),
            vec![(1, "sleep 30".to_string())],
            "the projection lists the session whose job is still active"
        );
        assert!(
            out.content.contains("Process running with session ID 1"),
            "the baseline wire is untouched: {}",
            out.content
        );
        w.ctx.sessions.shutdown().await;
    }

    /// US-136 AC2: the record is durable BEFORE the launcher runs. Asking the
    /// registry to launch a token nothing staged makes the launcher fail, and
    /// the log still carries the registration that preceded it, followed by the
    /// `Failed` the failure settled (US-135 AC5 through the real launcher).
    #[tokio::test]
    async fn the_registration_is_durable_even_when_the_launch_that_follows_fails() {
        let w = wired().await;

        let err = w
            .registry
            .register(agent_runtime::JobKind::Terminal, "npm run dev", 4096)
            .await
            .expect_err("nothing was staged under that token");
        assert!(
            err.to_string().contains("no staged command"),
            "the caller learns why: {err}"
        );

        let trace = job_trace(&w.store).await;
        assert_eq!(trace.len(), 2, "{trace:?}");
        assert_eq!(trace[0], "registered npm run dev");
        assert!(
            trace[1].starts_with("failed None"),
            "the failure follows the registration: {}",
            trace[1]
        );
        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(w.registry.active(), 0, "the slot is back");
    }

    /// US-136 AC7: the job identity is an ADDITION. The structured answer keeps
    /// exactly the baseline keys, and no `job_id` leaks into what Codex's models
    /// are trained to read.
    #[tokio::test]
    async fn the_baseline_wire_gains_no_field_from_the_registry() {
        let w = wired().await;

        let out = agent_tools::ExecCommand
            .call(exec("echo hi", 2000), &w.ctx)
            .await
            .expect("the command runs");

        let structured = out
            .structured_content
            .expect("the tool answers structured JSON");
        let mut keys: Vec<&str> = structured
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "chunk_id",
                "exit_code",
                "original_token_count",
                "output",
                "output_omitted_bytes",
                "session_id",
                "signal",
                "wall_time_seconds",
            ]
        );
        w.ctx.sessions.shutdown().await;
    }

    /// US-137 AC1 and AC7: exit zero settles `Completed` carrying the code, and
    /// the durable line is written BEFORE the tool answers, not after.
    #[tokio::test]
    async fn a_zero_exit_settles_completed_and_the_log_has_it_before_the_answer() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("true", 2000), &w.ctx)
            .await
            .expect("the command runs");

        // Read immediately: nothing below awaits the process again, so a settle
        // published after the answer would still be missing here.
        assert_eq!(
            job_trace(&w.store).await,
            vec![
                "registered true".to_string(),
                "completed Some(0) None".to_string()
            ]
        );
        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.exit_code, Some(0));
    }

    /// US-137 AC2: a non-zero exit is an ANSWER, not a failure. `Failed` stays
    /// reserved for the launch that never happened.
    #[tokio::test]
    async fn a_nonzero_exit_settles_completed_with_its_code_never_failed() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("exit 3", 2000), &w.ctx)
            .await
            .expect("the command runs");

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.exit_code, Some(3));
        assert_eq!(job.cause, None);
    }

    /// US-137 AC4: a process killed by a signal carries the signal in its
    /// detail and settles `Killed`.
    #[tokio::test]
    async fn a_signal_killed_process_settles_killed_naming_the_signal() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("kill -9 $$", 2000), &w.ctx)
            .await
            .expect("the command runs");

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed);
        assert_eq!(job.cause.as_deref(), Some("killed by signal 9"));
    }

    /// US-137 AC3 and AC5: a terminated session settles `Killed`, and a late
    /// outcome on the settled job changes nothing and panics on no one.
    #[tokio::test]
    async fn a_terminated_session_settles_killed_and_stays_killed() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("sleep 30", 250), &w.ctx)
            .await
            .expect("the session opens");
        agent_tools::WriteStdin
            .call(
                agent_tools::exec_session::WriteStdinInput {
                    session_id: 1,
                    chars: String::new(),
                    yield_time_ms: Some(250),
                    max_output_tokens: None,
                    terminate: Some(true),
                },
                &w.ctx,
            )
            .await
            .expect("the session is terminated");

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed);
        let cause = job.cause.clone().expect("a killed job names its cause");

        let late = w
            .registry
            .settle(
                job.job_id,
                agent_runtime::JobOutcome::Completed { exit_code: 0 },
            )
            .await
            .expect("a late outcome is not an error");
        assert!(late.is_none(), "the first arrival keeps the job");
        let after = only_job(&w.registry);
        assert_eq!(after.status, JobStatus::Killed);
        assert_eq!(after.cause, Some(cause));
        assert!(
            w.ctx.sessions.open_sessions().is_empty(),
            "a settled job leaves the projection"
        );
    }

    /// US-137 AC1 and AC2: a process that finished while nobody polled it still
    /// finished. The run ending OBSERVES that exit, it does not cause it, so
    /// the job carries its code instead of being reported as killed.
    #[tokio::test]
    async fn a_terminal_that_exited_unpolled_settles_completed_at_the_end_of_the_run() {
        let w = wired().await;

        // The yield expires while the shell is still running, so the tool
        // answers a session it leaves open. Nothing polls it again: the exit is
        // only observed when the run ends.
        agent_tools::ExecCommand
            .call(exec("sleep 1; exit 7", 250), &w.ctx)
            .await
            .expect("the session opens");
        assert_eq!(only_job(&w.registry).status, JobStatus::Running);
        tokio::time::sleep(Duration::from_millis(1_500)).await;

        w.ctx.sessions.shutdown().await;

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.exit_code, Some(7));
        assert_eq!(job.cause, None);
        assert_eq!(
            job_trace(&w.store).await,
            vec![
                "registered sleep 1; exit 7".to_string(),
                "completed Some(7) None".to_string()
            ]
        );
    }

    /// US-137 AC6: the run ends while a terminal is open. The job settles
    /// `Killed` with its cause, and the process does not outlive the run.
    #[tokio::test]
    async fn the_end_of_the_run_settles_an_open_terminal_killed() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("sleep 30", 250), &w.ctx)
            .await
            .expect("the session opens");
        w.ctx.sessions.shutdown().await;

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed);
        assert_eq!(
            job.cause.as_deref(),
            Some("killed: the run ended while the shell session was open")
        );
        assert!(w.ctx.sessions.is_empty());
    }

    /// A thread interrupt reaches the shell itself, not only its accounting: the
    /// registry cancels, the launcher's process stops, and the job settles.
    #[tokio::test]
    async fn cancelling_a_job_stops_the_terminal_behind_it() {
        let w = wired().await;

        agent_tools::ExecCommand
            .call(exec("sleep 30", 250), &w.ctx)
            .await
            .expect("the session opens");
        let job_id = only_job(&w.registry).job_id;

        w.registry
            .cancel(job_id, "stopped by the user")
            .await
            .expect("the job is stopped");

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed);
        assert!(job.reported, "the owner was told before the stop");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if w.ctx.sessions.drain_output(1).is_err() || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        w.ctx.sessions.shutdown().await;
    }
}
