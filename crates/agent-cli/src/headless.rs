//! Headless one-shot run, on the thread runtime (US-018).
//!
//! `pyxis -p` no longer drives `run_agent` itself: it opens (or resumes) a
//! thread, submits ONE input and waits for that turn's terminal state. What the
//! script observes therefore carries the same identifiers, the same transitions
//! and the same durable ordering as an interactive session, and Ctrl+C reaches
//! the model, the tools and their process trees through the same cancellation
//! tree instead of killing the process mid-write.

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::AgentEvent;
use agent_runtime::TurnState;
use agent_runtime::thread::{RuntimeEventPayload, Submission};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::jsonl::{self, EventIdentity, RunEnd};
use crate::runtime::{CliStepSource, CommittedText, EngineDeps, SessionRuntime, SettingsCell};

/// Everything the binary resolved before the sandbox and the runtime.
pub struct HeadlessRun<'a> {
    pub prompt: String,
    /// `None` under `--ephemeral`: the thread lives in memory and no file of any
    /// kind is created (AC4).
    pub session_path: Option<PathBuf>,
    pub session_id: String,
    pub workspace: PathBuf,
    pub output_format: jsonl::OutputFormat,
    pub output_last_message: Option<String>,
    pub hooks: Arc<dyn agent_tools::hooks::Hooks>,
    pub skills: &'a crate::skills::Catalog,
    pub registry: Arc<agent_tools::Registry>,
    pub engine: EngineDeps,
    pub settings: Arc<SettingsCell>,
    pub steps: Arc<CliStepSource>,
}

/// Runs the turn and returns once its terminal state is durable.
pub async fn run(run: HeadlessRun<'_>) -> anyhow::Result<()> {
    let HeadlessRun {
        prompt,
        session_path,
        session_id,
        workspace,
        output_format,
        output_last_message,
        hooks,
        skills,
        registry,
        engine,
        settings,
        steps,
    } = run;

    // US-017 AC2 of the harness PRD: the prompt is submitted to the hooks before
    // it becomes a turn. A refusal names the reason and no thread is opened.
    if let agent_tools::hooks::HookDecision::Deny(reason) = hooks
        .lifecycle(agent_tools::hooks::Lifecycle::UserPromptSubmit { prompt: &prompt })
        .await
    {
        anyhow::bail!("hook: {reason}");
    }
    // A prompt opening on `/<skill>` injects that skill's instructions. An
    // unreadable skill fails the run instead of sending a turn that would only
    // carry its name.
    let injection = match crate::skills::invocation(skills, &prompt) {
        Some(Ok(injection)) => Some(injection),
        Some(Err(err)) => anyhow::bail!("skill: {err}"),
        None => None,
    };

    // Root of this run's cancellation tree. Every thread, turn, tool and process
    // hangs from it, so a single `cancel()` reaches all of them.
    let cancel = CancellationToken::new();
    let runtime = SessionRuntime::open(
        session_path.as_deref(),
        engine,
        registry,
        settings,
        Arc::clone(&steps),
        &cancel,
    )
    .await?;
    if let Some(injection) = injection {
        steps.inject(format!("skill:{}", injection.name), injection.block);
    }

    let thread_id = runtime.thread_id().to_string();
    let mut events = runtime.subscribe();
    let mut writer = jsonl::EventWriter::new(output_format);
    // Reference taken BEFORE the turn, on the workspace as it is. Machine output
    // only: the text format must stay identical to the character, so it has no
    // consumer for this diff and must not pay for a `git status` per run.
    let mut diff_tracker = if writer.is_json() {
        Some(agent_tools::turn_diff::TurnDiffTracker::begin(&workspace).await)
    } else {
        None
    };

    let accepted = runtime
        .submit(Submission {
            text: prompt,
            client_message_id: None,
        })
        .await
        .map_err(|err| anyhow::anyhow!("submit: {err}"))?;
    let turn_id = accepted.turn_id;
    let identity = EventIdentity {
        thread_id: Some(thread_id.clone()),
        turn_id: Some(turn_id.to_string()),
        event_id: None,
    };

    let mut text = CommittedText::default();
    let mut signalled = false;
    let ended = loop {
        tokio::select! {
            biased;
            received = events.recv() => match received {
                Ok(event) => {
                    if let Some(end) = observe(&event, turn_id, &mut writer, &mut text, &thread_id) {
                        break end;
                    }
                }
                // The durable state is the store, so a dropped LIVE event costs
                // the stream a line, not the run. Reported rather than hidden.
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    eprintln!("[runtime] {dropped} event(s) dropped from the live stream");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break terminal_from_status(&runtime, turn_id);
                }
            },
            // AC5: Ctrl+C goes through the runtime. The turn stops cooperatively,
            // reconciles its transcript and writes ITS terminal; the process does
            // not exit from under a half-written log.
            signal = tokio::signal::ctrl_c(), if !signalled => {
                signalled = true;
                if signal.is_ok() {
                    eprintln!("[runtime] interrupting the current turn...");
                    let _ = runtime.interrupt(Some(turn_id)).await;
                }
            }
        }
    };

    steps.clear_injections();
    // Aggregated diff after the end of the turn, hence after the last tool write
    // (including when the turn was interrupted).
    if let Some(tracker) = diff_tracker.as_mut() {
        match tracker.turn_diff().await {
            Ok(diff) if !diff.is_empty() => {
                writer.identified_event(&AgentEvent::TurnDiff(diff), &identity)
            }
            Ok(_) => {}
            Err(err) => eprintln!("[diff] {err}"),
        }
    }
    writer.run_summary(&session_id, &ended, &identity);

    // The session is over for the runtime before the process decides its exit
    // code: the store closes, the tasks are drained, and nothing keeps writing
    // behind the summary line.
    runtime.shutdown().await;

    // `Stop` fires when the agent really stops, so not on an interruption or a
    // failure, which the client owns.
    if ended == RunEnd::EndTurn {
        hooks.lifecycle(agent_tools::hooks::Lifecycle::Stop).await;
    }

    let answer = text
        .into_text()
        .replace(crate::interactive::GOAL_DONE_MARKER, "")
        .trim_end()
        .to_string();
    // AC6: written whatever the outcome, because the exit code is what tells a
    // pipeline whether the run succeeded; the failure of the WRITE is reported
    // after the outcome, which owns the exit code. A destination outside the
    // writable perimeter is refused by the sandbox, not widened for it.
    let last_message_write = output_last_message.map(|path| {
        std::fs::write(&path, &answer)
            .map_err(|err| anyhow::anyhow!("--output-last-message {path}: {err}"))
    });

    match &ended {
        RunEnd::EndTurn => {}
        RunEnd::Interrupted(_) => anyhow::bail!("interrupted"),
        RunEnd::Exhausted(reason) => anyhow::bail!("stopped: {reason}"),
        RunEnd::Error(err) => anyhow::bail!("{err}"),
    }
    if let Some(written) = last_message_write {
        written?;
    }
    // In JSON mode the text already lives in the `text` events: writing it again
    // would inject non-JSON lines into the stream.
    if !writer.is_json() {
        print!("{answer}");
        if !answer.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// Routes one runtime event and reports the terminal state when it names one.
fn observe(
    event: &agent_runtime::thread::RuntimeEvent,
    turn_id: agent_runtime::TurnId,
    writer: &mut jsonl::EventWriter,
    text: &mut CommittedText,
    thread_id: &str,
) -> Option<RunEnd> {
    match &event.payload {
        RuntimeEventPayload::Engine(engine) => {
            text.observe(engine);
            writer.identified_event(
                engine,
                &EventIdentity {
                    thread_id: Some(thread_id.to_string()),
                    turn_id: event.turn_id.map(|id| id.to_string()),
                    event_id: Some(event.event_id.to_string()),
                },
            );
            None
        }
        RuntimeEventPayload::TurnStateChanged { to, cause, .. }
            if to.is_terminal() && event.turn_id == Some(turn_id) =>
        {
            Some(RunEnd::from_terminal(*to, cause.clone()))
        }
        _ => None,
    }
}

/// Terminal state read back from the last-state signal.
///
/// The live stream can close before the terminal event is observed (a runtime
/// shut down from under the client). The status is the same fact, read from the
/// other channel, so the run still reports what happened instead of an
/// unexplained success.
fn terminal_from_status(runtime: &SessionRuntime, turn_id: agent_runtime::TurnId) -> RunEnd {
    match runtime.status().turn {
        Some(status) if status.turn_id == turn_id && status.state.is_terminal() => {
            RunEnd::from_terminal(status.state, None)
        }
        _ => RunEnd::from_terminal(
            TurnState::Interrupted,
            Some("runtime stopped before the turn reached a terminal state".into()),
        ),
    }
}
