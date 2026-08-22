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

use agent_runtime::jobs::{JobLaunch, JobLauncher, JobOutput, JobProcess};
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
    /// The transcript of the session, not its cursor.
    ///
    /// `write_stdin` keeps the consuming read that makes its poll loop advance;
    /// what the registry hands the model is the whole output, and handing it
    /// twice hands the same bytes (US-139 AC1/AC2).
    async fn read_output(&self) -> Result<JobOutput, String> {
        self.sessions
            .transcript(self.session_id)
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
    use agent_runtime::jobs::{CompletionDelivery, JobRegistry, JobStatus};
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
        wired_with(CompletionDelivery::Quiet).await
    }

    /// The same wiring, with the completion delivery of the client under test.
    /// `Quiet` is what `-p` and the app-server pass; `Wake` is the interactive
    /// loop (EP-044).
    async fn wired_with(delivery: CompletionDelivery) -> Wired {
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
            completion_delivery: delivery,
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

    /// Every input the durable log carries, oldest first. A turn opened by a
    /// completion is an input like any other, which is exactly why this reads
    /// the log rather than a counter.
    async fn inputs(store: &Arc<dyn ThreadStore>) -> Vec<(Option<String>, String)> {
        store
            .read()
            .await
            .expect("the store reads")
            .events
            .iter()
            .filter_map(|event| match &event.payload {
                ThreadEventPayload::InputSubmitted {
                    client_message_id,
                    text,
                    ..
                } => Some((client_message_id.clone(), text.clone())),
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

    /// Calls `list_jobs` exactly as the binary registers it: on the job handle
    /// the terminal tools already carry, which the thread above bound.
    async fn list_jobs(w: &Wired, job_id: Option<&str>) -> Result<String, agent_tools::ToolError> {
        agent_tools::ListJobs::new(w.ctx.sessions.job_handle())
            .call(
                agent_tools::jobs::ListJobsInput {
                    job_id: job_id.map(str::to_string),
                },
                &w.ctx,
            )
            .await
            .map(|out| out.content)
    }

    /// Polls a session through the REAL `write_stdin` empty poll, which is how
    /// a model observes an exit: it is the call that reaches `finish`, settles
    /// the job and hands the model its result.
    async fn poll(w: &Wired, session_id: u64, yield_ms: u64) -> String {
        agent_tools::WriteStdin
            .call(
                agent_tools::exec_session::WriteStdinInput {
                    session_id,
                    chars: String::new(),
                    yield_time_ms: Some(yield_ms),
                    max_output_tokens: None,
                    terminate: None,
                },
                &w.ctx,
            )
            .await
            .map(|out| out.content)
            .unwrap_or_default()
    }

    /// Polls until the single job of the thread is terminal, or gives up.
    async fn settle(w: &Wired, session_id: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !only_job(&w.registry).status.is_terminal()
        {
            poll(w, session_id, 100).await;
        }
        assert!(
            only_job(&w.registry).status.is_terminal(),
            "the job never settled"
        );
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
                // US-140: the answer that carries the exit code IS the report,
                // so the flag is durable before the settle it belongs to.
                "reported".to_string(),
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
            if w.ctx.sessions.transcript(1).is_err() || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        w.ctx.sessions.shutdown().await;
    }

    // ───────── EP-043: what the model sees ─────────

    /// US-138 AC2 and edge case 18: an empty registry ANSWERS. A tool that
    /// returned nothing would read like a broken call, and a model would either
    /// retry it or conclude the feature does not exist.
    #[tokio::test]
    async fn list_jobs_on_a_thread_that_started_nothing_says_so() {
        let w = wired().await;

        let out = list_jobs(&w, None).await.expect("the tool answers");

        assert_eq!(out, "no background job");
        w.ctx.sessions.shutdown().await;
    }

    /// US-138 AC3 and the Technical Consideration on identifiers: one job, two
    /// names. The opaque `job_...` is what the registry mints; the session id is
    /// what `write_stdin` takes. A listing that carried only one of them would
    /// leave the model holding a handle it cannot use.
    #[tokio::test]
    async fn a_listed_job_carries_both_of_its_identifiers_its_state_and_its_command() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 120), &w.ctx)
            .await
            .expect("the session opens");
        let job = only_job(&w.registry);

        let out = list_jobs(&w, None).await.expect("the tool answers");

        assert!(out.contains(&job.job_id.to_string()), "{out}");
        assert!(out.contains("session=1"), "{out}");
        assert!(out.contains("kind=terminal"), "{out}");
        assert!(out.contains("[running]"), "{out}");
        assert!(out.contains("command: sleep 30"), "{out}");
        w.ctx.sessions.shutdown().await;
    }

    /// US-138 AC4 and edge case 19: the command is text the MODEL wrote, and a
    /// listing renders it straight back to the model and to a terminal. An
    /// escape sequence must not survive the trip, and a command longer than the
    /// line budget must be cut without ever splitting a glyph.
    #[tokio::test]
    async fn a_listed_command_is_neutralized_and_bounded() {
        let w = wired().await;
        // A real escape sequence plus a multi-byte tail, so the cut lands
        // inside a two-byte character if the boundary is ever ignored.
        let command = format!("sleep 30 # \x1b[2J\x07 {}", "é".repeat(200));
        agent_tools::ExecCommand
            .call(exec(&command, 120), &w.ctx)
            .await
            .expect("the session opens");

        let out = list_jobs(&w, None).await.expect("the tool answers");

        assert!(!out.contains('\x1b'), "no escape survives: {out:?}");
        assert!(!out.contains('\x07'), "no bell survives: {out:?}");
        let rendered = out
            .lines()
            .find_map(|line| line.trim().strip_prefix("command: "))
            .expect("the listing carries a command line");
        assert!(
            rendered.ends_with("..."),
            "the cut is announced: {rendered}"
        );
        assert!(
            rendered.len() <= 163,
            "the command stays within its byte budget, got {}",
            rendered.len()
        );
        w.ctx.sessions.shutdown().await;
    }

    /// US-138 AC6: listing is an OBSERVATION. It settles nothing, marks
    /// nothing, and frees no slot, so a model may call it between two turns
    /// without changing what the next one sees.
    #[tokio::test]
    async fn listing_jobs_changes_nothing_about_them() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 120), &w.ctx)
            .await
            .expect("the session opens");
        let before = only_job(&w.registry);
        let trace_before = job_trace(&w.store).await;

        let first = list_jobs(&w, None).await.expect("the tool answers");
        let second = list_jobs(&w, None).await.expect("the tool answers again");

        assert_eq!(first, second, "a second listing sees the same world");
        let after = only_job(&w.registry);
        assert_eq!(after.status, before.status);
        assert!(!after.reported, "listing is not reporting");
        assert_eq!(w.registry.active(), 1, "no slot moved");
        assert_eq!(
            job_trace(&w.store).await,
            trace_before,
            "listing wrote nothing durable"
        );
        w.ctx.sessions.shutdown().await;
    }

    /// US-138 AC7: a thread with no registry gets a NAMED refusal. An empty
    /// list would say "this thread runs nothing", which is a different fact and
    /// the one a model would act on.
    #[tokio::test]
    async fn list_jobs_without_a_registry_refuses_by_name_instead_of_lying() {
        let dir = std::env::temp_dir();
        let sessions = agent_tools::ExecSessions::new();
        let mut ctx = ToolCtx::new(dir);
        ctx.sessions = sessions.clone();

        let err = agent_tools::ListJobs::new(sessions.job_handle())
            .call(agent_tools::jobs::ListJobsInput { job_id: None }, &ctx)
            .await
            .expect_err("nothing is bound");

        assert!(
            err.to_string()
                .contains("no registry is bound to this thread"),
            "the refusal names its cause: {err}"
        );
    }

    /// US-138 AC5: waiting on a background job IS calling this twice, so the
    /// loop guard must not count the repetition as a model going in circles.
    #[test]
    fn list_jobs_is_exempt_from_the_loop_guard() {
        let tool = agent_tools::ListJobs::new(Arc::new(agent_tools::JobHandle::new()));
        assert!(tool.loop_guard_exempt(&serde_json::json!({"job_id": null})));
        assert!(tool.loop_guard_exempt(&serde_json::json!({"job_id": "job_00"})));
    }

    /// US-138 AC1: the policy properties are the ones `list_agents` declares.
    /// They are what the catalog renders and what the parallel segment reads,
    /// so they are asserted rather than described.
    #[test]
    fn list_jobs_declares_the_policy_of_a_reader() {
        let tool = agent_tools::ListJobs::new(Arc::new(agent_tools::JobHandle::new()));
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert!(!tool.is_sensitive());
        assert!(!tool.is_taint_sensitive());
        assert!(
            tool.returns_untrusted(),
            "a command line and a process output are both untrusted text"
        );
    }

    /// US-139 AC1/AC2, the split the whole story exists for: `write_stdin`
    /// keeps its consuming cursor, and the final relève is idempotent. Two
    /// readers on one stream that both consumed would divide it between them.
    #[tokio::test]
    async fn the_final_output_survives_the_consuming_cursor_and_is_the_same_bytes_twice() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("echo pyxis-marker; exit 0", 800), &w.ctx)
            .await
            .expect("the session opens");
        settle(&w, 1).await;
        let job = only_job(&w.registry);

        let first = list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads");
        let second = list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads again");

        assert!(first.contains("pyxis-marker"), "{first}");
        assert_eq!(first, second, "the same relève returns the same bytes");
        w.ctx.sessions.shutdown().await;
    }

    /// US-139 AC1 from the other side: the incremental cursor still empties.
    /// `exec_command` consumed the first chunk, and what it consumed is still
    /// in the relève, so neither reader is stealing from the other.
    #[tokio::test]
    async fn the_consuming_cursor_of_write_stdin_keeps_its_own_behavior() {
        let w = wired().await;
        let opened = agent_tools::ExecCommand
            .call(exec("echo first-chunk; sleep 30", 800), &w.ctx)
            .await
            .expect("the session opens");
        assert!(
            opened.content.contains("first-chunk"),
            "the cursor delivered the chunk: {}",
            opened.content
        );

        let polled = poll(&w, 1, 150).await;
        let job = only_job(&w.registry);
        let relieve = list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads");

        assert!(
            !polled.contains("first-chunk"),
            "a consumed chunk is not served twice by the cursor: {polled}"
        );
        assert!(
            relieve.contains("first-chunk"),
            "the transcript still carries it: {relieve}"
        );
        assert!(relieve.contains("still running"), "{relieve}");
        w.ctx.sessions.shutdown().await;
    }

    /// US-139 AC5: a job still running answers with what it has, on the yield
    /// bound that already exists. It must not block waiting for an exit.
    #[tokio::test]
    async fn a_running_job_answers_without_waiting_for_its_exit() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 50), &w.ctx)
            .await
            .expect("the session opens");
        let job = only_job(&w.registry);

        let started = tokio::time::Instant::now();
        let out = list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the relève returned promptly, took {:?}",
            started.elapsed()
        );
        assert!(out.contains("[running]"), "{out}");
        assert!(out.contains("still running"), "{out}");
        assert!(!job.reported, "a poll is not a result");
        w.ctx.sessions.shutdown().await;
    }

    /// US-139 AC6 and edge case 9: a process that writes bytes which are not
    /// UTF-8 must not take the relève down with it. The replacement policy is
    /// the one the terminals already apply, `from_utf8_lossy`, so an invalid
    /// sequence becomes U+FFFD and the exit code still reaches the model.
    #[tokio::test]
    async fn non_utf8_output_is_replaced_rather_than_fatal() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("printf 'a\\377b'; exit 3", 800), &w.ctx)
            .await
            .expect("the session opens");
        settle(&w, 1).await;
        let job = only_job(&w.registry);

        let out = list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads");

        assert!(
            out.contains('\u{fffd}'),
            "the invalid byte is replaced: {out:?}"
        );
        assert!(out.contains("exit_code=3"), "{out}");
        w.ctx.sessions.shutdown().await;
    }

    /// Edge case 20: a job of ANOTHER thread reads as unknown, never as
    /// refused. The two answers would let a probe tell a foreign thread's job
    /// apart from an identifier that never existed.
    #[tokio::test]
    async fn a_job_of_another_thread_reads_as_unknown() {
        let mine = wired().await;
        let other = wired().await;
        other
            .ctx
            .sessions
            .job_handle()
            .bind(Arc::clone(&other.registry));
        agent_tools::ExecCommand
            .call(exec("sleep 30", 120), &other.ctx)
            .await
            .expect("the session opens");
        let foreign = only_job(&other.registry).job_id;

        let err = list_jobs(&mine, Some(&foreign.to_string()))
            .await
            .expect_err("it is not reachable from here");

        assert!(
            err.to_string().contains("not reachable from this thread"),
            "unknown, not forbidden: {err}"
        );
        mine.ctx.sessions.shutdown().await;
        other.ctx.sessions.shutdown().await;
    }

    /// US-140 AC1 and AC5: the flag flips when the result REACHES the model,
    /// and the durable `JobReported` line is written at the flip so a reopened
    /// thread reads it back instead of re-deriving it.
    #[tokio::test]
    async fn a_collected_result_marks_the_job_reported_once_and_durably() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 50), &w.ctx)
            .await
            .expect("the session opens");
        // Settled by the teardown, which hands nothing to the model: the job is
        // terminal and still unread, which is the only state the flag can flip
        // from.
        w.ctx.sessions.shutdown().await;
        let job = only_job(&w.registry);
        assert!(job.status.is_terminal(), "the teardown settled it");
        assert!(!job.reported, "nothing collected yet");

        list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("the result reads");

        assert!(only_job(&w.registry).reported, "the model got the result");
        list_jobs(&w, Some(&job.job_id.to_string()))
            .await
            .expect("reading it twice is allowed");
        let reported = job_trace(&w.store)
            .await
            .into_iter()
            .filter(|line| line == "reported")
            .count();
        assert_eq!(reported, 1, "one durable line, written at the flip");
        // What a reopened thread makes of that line is proved on the resume
        // harness itself, in `crates/agent-runtime/tests/jobs.rs`.
        w.ctx.sessions.shutdown().await;
    }

    /// US-140 AC7: the counting test. Fifty turns after a settle announce the
    /// job exactly once, which is the regression Claude Code #12302 and #13249
    /// describe in reverse.
    #[tokio::test]
    async fn over_fifty_turns_a_settled_job_is_announced_exactly_once() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 50), &w.ctx)
            .await
            .expect("the session opens");
        // Settled without any reader: the thread is torn down while the job
        // runs, which is the case that owes the user an announcement.
        w.ctx.sessions.shutdown().await;

        let mut announcements = 0;
        for _ in 0..50 {
            if let Some(notice) = crate::interactive::finished_jobs_notice(&w.ctx.sessions) {
                announcements += 1;
                let job = only_job(&w.registry);
                assert!(notice.contains(&job.job_id.to_string()), "{notice}");
                // What a turn does with the notice: the result reaches its
                // reader, so the job owes nobody a second announcement.
                w.registry
                    .mark_reported(job.job_id)
                    .await
                    .expect("the job is ours");
            }
        }

        assert_eq!(announcements, 1, "announced once over fifty turns");
    }

    /// US-140 AC3 and AC4 together, through the notice the interactive loop
    /// really composes: a job whose result reached the model is silent, and a
    /// finished job nobody read is named. A result is never lost quietly, and a
    /// collected one never repeats itself.
    #[tokio::test]
    async fn the_notice_names_the_unread_results_and_only_those() {
        let collected = wired().await;
        agent_tools::ExecCommand
            .call(exec("echo done; exit 0", 800), &collected.ctx)
            .await
            .expect("the session opens");
        settle(&collected, 1).await;

        assert!(
            only_job(&collected.registry).reported,
            "finishing inside the call handed the model its result"
        );
        assert_eq!(
            crate::interactive::finished_jobs_notice(&collected.ctx.sessions),
            None,
            "a reported job is not announced again"
        );

        let unread = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 50), &unread.ctx)
            .await
            .expect("the session opens");
        unread.ctx.sessions.shutdown().await;

        let notice = crate::interactive::finished_jobs_notice(&unread.ctx.sessions)
            .expect("a finished job nobody read is announced");
        assert!(notice.contains("finished unread"), "{notice}");
        assert!(notice.contains("list_jobs"), "{notice}");
        collected.ctx.sessions.shutdown().await;
    }

    /// US-140 AC6: a KILLED job counts as reported once the acknowledgment
    /// reached the model, exactly like one that exited on its own. The stop
    /// path already writes the flag before it attempts the kill.
    #[tokio::test]
    async fn a_killed_job_is_reported_like_a_finished_one() {
        let w = wired().await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 120), &w.ctx)
            .await
            .expect("the session opens");
        let job_id = only_job(&w.registry).job_id;

        w.registry
            .cancel(job_id, "stopped by the user")
            .await
            .expect("the job stops");

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed);
        assert!(job.reported, "the acknowledgment reached its owner");
        assert_eq!(
            crate::interactive::finished_jobs_notice(&w.ctx.sessions),
            None,
            "a killed and acknowledged job is not announced a second time"
        );
        w.ctx.sessions.shutdown().await;
    }

    /// US-139 AC3: an output over the model cap goes through the EXISTING
    /// spill of ADR-15 and reaches the model as a path, with no second bound of
    /// its own. Dispatched through a real `Registry` with a real spill store,
    /// because the decision point is there and nowhere in the tool.
    #[tokio::test]
    async fn an_oversized_relieve_goes_through_the_existing_spill() {
        let w = wired().await;
        // Comfortably over MAX_TOOL_OUTPUT_BYTES and well under MAX_JOB_OUTPUT,
        // so what the model reads is decided by the spill and not by a cap of
        // the registry.
        agent_tools::ExecCommand
            .call(exec("seq 1 12000; exit 0", 2000), &w.ctx)
            .await
            .expect("the session opens");
        settle(&w, 1).await;
        let job_id = only_job(&w.registry).job_id;

        let spill_dir =
            std::env::temp_dir().join(format!("pyxis-jobs-spill-{}", std::process::id()));
        std::fs::create_dir_all(&spill_dir).expect("a spill root");
        let store = Arc::new(
            agent_tools::SpillStore::create(&spill_dir, "thread_jobs")
                .expect("the spill store opens"),
        );
        let registry = agent_tools::Registry::builder(&spill_dir)
            .spill(Arc::clone(&store))
            .register(agent_tools::ListJobs::new(w.ctx.sessions.job_handle()))
            .build();

        let results = registry
            .dispatch(vec![agent_core::tools::ToolInvocation::json(
                "c1",
                "list_jobs",
                serde_json::json!({ "job_id": job_id.to_string() }),
            )])
            .await;

        let result = results.first().expect("one result");
        assert!(!result.is_error, "a spill is not a failure: {result:?}");
        let truncation = result
            .truncation
            .as_ref()
            .expect("the oversized relève was spilled");
        assert!(
            truncation.original_bytes > agent_tools::tool::MAX_TOOL_OUTPUT_BYTES,
            "the spill fired on the size the tools already agree on"
        );
        assert!(
            result.content.len() <= agent_tools::tool::MAX_TOOL_OUTPUT_BYTES,
            "what the model reads fits the cap, got {}",
            result.content.len()
        );
        w.ctx.sessions.shutdown().await;
        let _ = std::fs::remove_dir_all(&spill_dir);
    }

    /// US-144 AC1/AC2: the real teardown of a headless run. `-p` calls
    /// `ExecSessions::shutdown` on its way out, which settles the terminal
    /// nobody polled. The settle is durable BEFORE the process leaves, and it
    /// opens no turn: there is no delivery half attached at all under `Quiet`,
    /// so FR-14 holds by absence of a caller rather than by a branch.
    #[tokio::test]
    async fn a_quiet_client_settles_its_jobs_on_the_way_out_and_opens_no_turn() {
        let w = wired_with(CompletionDelivery::Quiet).await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 100), &w.ctx)
            .await
            .expect("the session opens");

        w.ctx.sessions.shutdown().await;

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Killed, "the run ended under it");
        assert!(
            !job.reported,
            "nobody read the result, so the job stays announceable on a resume"
        );
        assert!(
            job_trace(&w.store)
                .await
                .iter()
                .any(|line| line.starts_with("killed")),
            "the terminal state is durable before the process exits: {:?}",
            job_trace(&w.store).await
        );
        assert!(
            inputs(&w.store).await.is_empty(),
            "a script has nobody to wake: {:?}",
            inputs(&w.store).await
        );
    }

    /// The other half of the same path, and the reason the assertion above is
    /// about the client and not about the teardown: the SAME settle, on a
    /// `Wake` client, opens exactly one turn keyed on the job. The announcement
    /// follows the commit, so the terminal line is already in the log when the
    /// input lands (US-142 AC2/AC5/AC6).
    #[tokio::test]
    async fn a_waking_client_turns_the_same_settle_into_one_turn_keyed_on_the_job() {
        let w = wired_with(CompletionDelivery::Wake).await;
        agent_tools::ExecCommand
            .call(exec("sleep 30", 100), &w.ctx)
            .await
            .expect("the session opens");
        let job_id = only_job(&w.registry).job_id;

        w.ctx.sessions.shutdown().await;

        let inputs = inputs(&w.store).await;
        assert_eq!(inputs.len(), 1, "one announcement, not two: {inputs:?}");
        let (client_message_id, text) = &inputs[0];
        assert_eq!(
            client_message_id.as_deref(),
            Some(format!("job-completion:{job_id}").as_str()),
            "the idempotency key is derived from the job identifier"
        );
        assert!(
            text.contains(&job_id.to_string()) && text.contains("list_jobs"),
            "the notice names the job and where its output is: {text}"
        );
        assert!(
            only_job(&w.registry).reported,
            "a delivered announcement marks the job reported"
        );
        let trace = job_trace(&w.store).await;
        let killed = trace
            .iter()
            .position(|line| line.starts_with("killed"))
            .expect("the terminal state is in the log");
        let reported = trace
            .iter()
            .position(|line| line == "reported")
            .expect("the report is in the log");
        assert!(
            killed < reported,
            "the announcement follows the commit: {trace:?}"
        );
    }

    /// US-142 AC1, the case the story is named after: the model launched a
    /// build, went on to something else, and the build finished. No poll, no
    /// teardown, no tool call of any kind happens between the launch and the
    /// assertions, so the only thing that can settle this job is the watch the
    /// session carries. The turn that follows is the proof it ran.
    #[tokio::test]
    async fn a_job_that_ends_while_nobody_polls_it_settles_itself_and_opens_a_turn() {
        let w = wired_with(CompletionDelivery::Wake).await;
        // The yield expires long before the command does, so the tool answers
        // an open session and the model is free to work elsewhere.
        agent_tools::ExecCommand
            .call(exec("sleep 0.6; exit 7", 100), &w.ctx)
            .await
            .expect("the session opens");
        let job_id = only_job(&w.registry).job_id;
        assert_eq!(only_job(&w.registry).status, JobStatus::Running);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline && inputs(&w.store).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let job = only_job(&w.registry);
        assert_eq!(job.status, JobStatus::Completed, "the exit was observed");
        assert_eq!(job.exit_code, Some(7), "with the code the shell returned");
        let inputs = inputs(&w.store).await;
        assert_eq!(inputs.len(), 1, "one turn, not one per tick: {inputs:?}");
        assert_eq!(
            inputs[0].0.as_deref(),
            Some(format!("job-completion:{job_id}").as_str()),
            "keyed on the job, so a redelivery re-executes nothing"
        );
        w.ctx.sessions.shutdown().await;
    }

    /// The other half of the same watch: it settles the job and closes NOTHING.
    /// A model that comes back to its session after the wake still reads the
    /// output where it left it, because the store still holds the session and
    /// its buffer (US-015, unchanged by EP-044).
    #[tokio::test]
    async fn the_watch_settles_the_job_without_taking_the_session_away() {
        let w = wired_with(CompletionDelivery::Wake).await;
        agent_tools::ExecCommand
            // The bytes land AFTER the opening yield, so what the poll below
            // reads is what survived the settle, not what the first call left.
            .call(exec("sleep 0.6; echo watched", 100), &w.ctx)
            .await
            .expect("the session opens");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline && !only_job(&w.registry).status.is_terminal()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            only_job(&w.registry).status.is_terminal(),
            "the job settled"
        );

        // The real poll, on the same session id, after the settle.
        let out = poll(&w, 1, 100).await;
        assert!(
            out.contains("watched"),
            "the session is still readable: {out}"
        );
        w.ctx.sessions.shutdown().await;
    }
}
