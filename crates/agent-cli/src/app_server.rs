//! The binary's half of the app-server contract (EP-005).
//!
//! `agent-app-server` owns the wire and owns nothing of what it takes to build
//! a thread. This module is the other half: which file a conversation writes
//! to, how a thread is found by its identifier, and how the client's declared
//! tools enter the registry every other tool goes through.
//!
//! One thread at a time, on purpose. The tool registry, the Code Mode session
//! and the sub-agent handle of this process each belong to the thread that is
//! open (`bind_thread`, `AgentHandle::bind`), so a second live thread would
//! silently rebind them under the first. The protocol layer turns that into a
//! typed conflict rather than into a thread whose tools point elsewhere.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_app_server::host::{HostError, OpenThread, RuntimeHost, ThreadControl};
use agent_app_server::items::project_messages;
use agent_app_server::protocol::{DynamicToolSpec, ThreadItem};
use agent_runtime::id::{ThreadId, TurnId};
use agent_runtime::thread::{InputOrigin, Submission, SubmitError};
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    AgentWiring, CliStepSource, EngineDeps, JobWiring, SessionRuntime, SettingsCell,
};

/// Everything the binary resolved before the runtime, kept so a thread can be
/// opened on demand rather than at startup.
pub struct CliHostParts {
    pub sessions_dir: PathBuf,
    pub engine: EngineDeps,
    pub registry: Arc<agent_tools::Registry>,
    pub settings: Arc<SettingsCell>,
    pub steps: Arc<CliStepSource>,
    pub agents: Option<AgentWiring>,
    /// How a background job becomes a process, shared by every thread the
    /// server opens (one at a time: `max_open_threads` is 1).
    pub jobs: Option<JobWiring>,
    pub bridge: Arc<agent_app_server::ClientBridge>,
    pub cancel: CancellationToken,
}

pub struct CliHost {
    parts: CliHostParts,
    /// The thread that is open, so its transcript answers a history request
    /// without re-reading a file the runtime is writing.
    open: Mutex<Option<(ThreadId, Arc<SessionRuntime>)>>,
}

impl CliHost {
    pub fn new(parts: CliHostParts) -> Arc<Self> {
        Arc::new(Self {
            parts,
            open: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(ThreadId, Arc<SessionRuntime>)>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Session file bound to `thread_id`, when one is.
    fn path_of(&self, thread_id: &str) -> Option<PathBuf> {
        agent_session::list_sessions(&self.parts.sessions_dir, None)
            .into_iter()
            .find(|session| {
                session
                    .thread_id
                    .map(|id| id.to_string() == thread_id)
                    .unwrap_or(false)
            })
            .map(|session| self.parts.sessions_dir.join(session.id))
    }

    /// Opens the durable log at `path` and binds this process to it.
    async fn open_at(
        self: &Arc<Self>,
        path: PathBuf,
        dynamic: Vec<DynamicToolSpec>,
    ) -> Result<OpenThread, HostError> {
        // FR-14: the protocol has one direction for a new turn, `session/prompt`,
        // and a background job is not a client. Forced quiet here rather than at
        // the dispatch site, so opening a thread through this host can never arm
        // a delivery that would push a turn the client never asked for.
        let jobs = self.parts.jobs.clone().map(|wiring| JobWiring {
            delivery: agent_runtime::jobs::CompletionDelivery::Quiet,
            ..wiring
        });
        let runtime = SessionRuntime::open(
            Some(&path),
            self.parts.engine.clone(),
            Arc::clone(&self.parts.registry),
            Arc::clone(&self.parts.settings),
            Arc::clone(&self.parts.steps),
            &self.parts.cancel,
            self.parts.agents.as_ref(),
            jobs.as_ref(),
        )
        .await
        .map_err(|err| HostError::Internal(err.to_string()))?;
        let runtime = Arc::new(runtime);
        let thread_id = runtime.thread_id();
        // The Code Mode session follows the thread: opening one closes the
        // previous, so a resumed thread never replays a cell.
        self.parts.steps.bind_thread(&thread_id).await;

        // The client's tools enter the SAME registry as the built-in ones, at a
        // step boundary like any other staged change. A refused declaration is
        // named rather than silently absent.
        let (tools, rejected) = agent_app_server::dynamic_tools(&self.parts.bridge, &dynamic);
        let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
        for refusal in &rejected {
            tracing::warn!(
                target: "pyxis::app_server",
                tool = %refusal.name,
                reason = %refusal.reason,
                "dynamic tool refused"
            );
        }
        if !tools.is_empty() {
            self.parts.registry.stage_tools(tools);
        }

        let items = project_messages(&runtime.messages());
        let events = runtime.subscribe();
        *self.lock() = Some((thread_id, Arc::clone(&runtime)));
        Ok(OpenThread {
            thread_id: thread_id.to_string(),
            items,
            events,
            control: Arc::new(CliControl {
                runtime,
                host: Arc::clone(self),
                dynamic: names,
            }) as Arc<dyn ThreadControl>,
        })
    }

    /// Items of a thread, live one included.
    fn history(&self, thread_id: &str) -> Result<Vec<ThreadItem>, HostError> {
        // The open thread answers from the transcript the runtime holds: the
        // file is being written by that same runtime, and a reader racing its
        // writer would show a torn tail.
        if let Some((open, runtime)) = self.lock().as_ref()
            && open.to_string() == thread_id
        {
            return Ok(project_messages(&runtime.messages()));
        }
        let path = self
            .path_of(thread_id)
            .ok_or_else(|| HostError::UnknownThread(thread_id.to_string()))?;
        let resumed = agent_session::resume_file(&path)
            .map_err(|err| HostError::Internal(format!("session: {err}")))?;
        Ok(project_messages(&resumed.messages))
    }
}

/// The `RuntimeHost` the server drives.
///
/// Separate from [`CliHost`] because opening a thread has to hand an
/// `Arc<CliHost>` to the control it builds, which a `&self` method cannot
/// produce.
pub struct SharedHost(pub Arc<CliHost>);

#[async_trait::async_trait]
impl RuntimeHost for SharedHost {
    async fn start_thread(&self, dynamic: Vec<DynamicToolSpec>) -> Result<OpenThread, HostError> {
        let path = crate::interactive::new_session_path(&self.0.parts.sessions_dir);
        self.0.open_at(path, dynamic).await
    }

    async fn resume_thread(
        &self,
        thread_id: &str,
        dynamic: Vec<DynamicToolSpec>,
    ) -> Result<OpenThread, HostError> {
        let path = self
            .0
            .path_of(thread_id)
            .ok_or_else(|| HostError::UnknownThread(thread_id.to_string()))?;
        self.0.open_at(path, dynamic).await
    }

    async fn history(&self, thread_id: &str) -> Result<Vec<ThreadItem>, HostError> {
        self.0.history(thread_id)
    }
}

struct CliControl {
    runtime: Arc<SessionRuntime>,
    host: Arc<CliHost>,
    /// Tools this thread staged, removed when it closes.
    dynamic: Vec<String>,
}

#[async_trait::async_trait]
impl ThreadControl for CliControl {
    async fn submit(
        &self,
        text: String,
        client_message_id: Option<String>,
    ) -> Result<String, HostError> {
        self.runtime
            .submit(Submission {
                text,
                client_message_id,
                // A protocol client speaks for a person: `session/prompt` is
                // how that person's message reaches this thread.
                origin: InputOrigin::Human,
            })
            .await
            .map(|accepted| accepted.turn_id.to_string())
            .map_err(submit_error)
    }

    async fn steer(
        &self,
        text: String,
        client_message_id: Option<String>,
        expected_turn: Option<String>,
    ) -> Result<String, HostError> {
        let expected = expected_turn.map(|id| parse_turn(&id)).transpose()?;
        self.runtime
            .steer(
                Submission {
                    text,
                    client_message_id,
                    origin: InputOrigin::Human,
                },
                expected,
            )
            .await
            .map(|accepted| accepted.turn_id.to_string())
            .map_err(submit_error)
    }

    async fn interrupt(&self, turn_id: Option<String>) -> Result<(), HostError> {
        let turn = turn_id.map(|id| parse_turn(&id)).transpose()?;
        self.runtime.interrupt(turn).await.map_err(submit_error)
    }

    async fn close(&self) {
        if !self.dynamic.is_empty() {
            self.host.parts.registry.stage_removal(self.dynamic.clone());
            self.host.parts.registry.commit_staged();
        }
        self.runtime.shutdown().await;
        self.host.parts.steps.clear_injections();
        let mut open = self.host.lock();
        if matches!(open.as_ref(), Some((id, _)) if *id == self.runtime.thread_id()) {
            *open = None;
        }
    }
}

fn parse_turn(raw: &str) -> Result<TurnId, HostError> {
    raw.parse::<TurnId>().map_err(|_| HostError::StaleTurn)
}

/// Maps a runtime refusal onto the protocol. A stale turn is its own answer:
/// a client that steered a turn that ended must not see its correction become
/// a new turn.
fn submit_error(error: SubmitError) -> HostError {
    match error {
        SubmitError::StaleTurn { .. } => HostError::StaleTurn,
        other => HostError::Refused(other.to_string()),
    }
}
