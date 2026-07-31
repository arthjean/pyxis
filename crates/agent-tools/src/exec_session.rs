//! Terminal sessions (EP-004): `exec_command` opens one and runs a command in
//! it, `write_stdin` feeds it, polls it or terminates it. The split is Codex's
//! (`codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` and
//! `write_stdin.rs`): two tools, each idempotent to describe, instead of one
//! tool with a mode.
//!
//! **The wire is the baseline one** (US-014). Input: `cmd`, `workdir`, `shell`,
//! `tty`, `yield_time_ms`, `max_output_tokens`; result: `chunk_id`,
//! `wall_time_seconds`, `exit_code`, `session_id`, `original_token_count` and
//! `output`, rendered as the text Codex's models are trained to read
//! (`codex-rs/core/src/tools/context.rs`, `response_text`). A model that ran a
//! command through Codex therefore reads the same answer here.
//!
//! **Pipes AND PTY** (US-014 AC1/AC2). `tty: false` keeps the pipe session that
//! predates this epic; `tty: true` allocates a real pseudo-terminal, without
//! which a program that checks `isatty` takes another branch than the one it
//! takes under Codex. The terminal lives in [`crate::pty`], the policy here.
//!
//! **The confinement covers stdin too.** A session is a shell: without the same
//! guardrail on what is written to it, opening `sh` then typing the command
//! would bypass the protected-subpath check that `bash` goes through
//! (`guard_command_paths`). Both tools therefore run it, on the command and on
//! every stdin payload.
//!
//! **Nothing is spawned before the refusals** (US-014 AC4). An absent workdir, a
//! refused shell and the fifth session are decided before the process exists,
//! so a refusal never leaves one behind.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use agent_core::tools::ToolExecution;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_COMMAND_BYTES, Tool, ToolCtx, ToolOutput};

/// Concurrent sessions cap. A session holds a process and its reader task;
/// past a handful the model is juggling, not working.
pub const MAX_SESSIONS: usize = 4;
/// Inactivity past which a session is closed and its process tree killed.
/// Counted from the last read or write on the session.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Watchdog tick. Fine enough that a forgotten session does not survive long,
/// coarse enough to cost nothing.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// Yield bounds of the baseline (`shell_spec.rs`): a call waits at least
/// `MIN_YIELD` and at most `MAX_YIELD` for output.
const MIN_YIELD: Duration = Duration::from_millis(250);
const MAX_YIELD: Duration = Duration::from_millis(30_000);
/// Baseline defaults, per call shape: a command opens with 10 s, a write yields
/// after 250 ms, an empty poll waits 5 s in the background.
const DEFAULT_EXEC_YIELD: Duration = Duration::from_millis(10_000);
const DEFAULT_WRITE_YIELD: Duration = Duration::from_millis(250);
const DEFAULT_POLL_YIELD: Duration = Duration::from_millis(5_000);
/// Polling period of the output buffer during a yield.
const POLL: Duration = Duration::from_millis(50);
/// Cap of what one chunk hands back, and of what a session keeps between two
/// reads. Well under the 10 MiB per-result ceiling: the bound is what makes the
/// memory of a chatty process constant, and what is dropped is COUNTED.
const MAX_CHUNK_BYTES: usize = crate::tool::MAX_TOOL_OUTPUT_BYTES;
/// Baseline output budget, in the tokens the model asks for. Same 4 bytes per
/// token approximation as `codex-rs/utils/string/src/truncate.rs`.
const APPROX_BYTES_PER_TOKEN: u64 = 4;
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// Termination budget (US-015 AC2): `SIGTERM`, then `SIGKILL`, the whole thing
/// under the 2 s the story allows.
const TERMINATE_GRACE: Duration = Duration::from_millis(1_200);
const KILL_GRACE: Duration = Duration::from_millis(500);
/// Environment every session command runs with, adopted from the baseline
/// (`unified_exec/process_manager.rs`, `UNIFIED_EXEC_ENV`): a PTY makes programs
/// believe a human is watching, and their colors and pagers would then land in
/// the transcript as escape sequences and as a process waiting for a keypress.
const SESSION_ENV: &[(&str, &str)] = &[
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
];

/// State of the process behind a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Why a session id is no longer live. Kept after the session itself is gone so
/// a later call can be told WHICH of the three states it hit (US-015 AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosedCause {
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Idle,
    Terminated,
    Shutdown,
}

/// Output waiting to be read, with an EXACT count of what the cap dropped.
#[derive(Debug, Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    /// Bytes the cap dropped since the last read.
    omitted: u64,
    /// Bytes the process produced since the last read, before the cap.
    produced: u64,
}

impl OutputBuffer {
    fn push(&mut self, data: &[u8]) {
        self.produced = self.produced.saturating_add(data.len() as u64);
        self.bytes.extend_from_slice(data);
        self.trim_to(MAX_CHUNK_BYTES);
    }

    /// Merges a drained chunk into an accumulator, cap and counters included.
    fn absorb(&mut self, other: &OutputBuffer) {
        self.bytes.extend_from_slice(&other.bytes);
        self.produced = self.produced.saturating_add(other.produced);
        self.omitted = self.omitted.saturating_add(other.omitted);
        self.trim_to(MAX_CHUNK_BYTES);
    }

    /// Keeps the TAIL within `max` and counts what that costs. Same policy as
    /// `bash`: on a chatty process the end is what carries the diagnosis.
    fn trim_to(&mut self, max: usize) {
        if self.bytes.len() > max {
            let overflow = self.bytes.len() - max;
            self.bytes.drain(0..overflow);
            self.omitted = self.omitted.saturating_add(overflow as u64);
        }
    }

    fn take(&mut self) -> OutputBuffer {
        std::mem::take(self)
    }
}

/// How a session talks to its process.
enum SessionIo {
    Pipe {
        stdin: Option<tokio::process::ChildStdin>,
    },
    #[cfg(unix)]
    Pty { master: Arc<crate::pty::PtyMaster> },
}

/// Handle taken out of the store for the duration of a write, so the mutex is
/// never held across an await.
enum SessionWriter {
    Pipe(tokio::process::ChildStdin),
    #[cfg(unix)]
    Pty(Arc<crate::pty::PtyMaster>),
}

/// One live terminal. Dropping it kills the process GROUP: the session outlives
/// a tool call, so nothing else would.
struct Session {
    pid: Option<u32>,
    child: Option<tokio::process::Child>,
    io: SessionIo,
    buffer: Arc<Mutex<OutputBuffer>>,
    status: SessionStatus,
    last_activity: Instant,
    command: String,
    /// Monotonic chunk counter (US-015 AC1): the model can order two reads of
    /// the same session without comparing their content.
    next_chunk: u64,
}

impl Session {
    fn status(&mut self) -> SessionStatus {
        if self.status == SessionStatus::Running {
            let observed = self.child.as_mut().map(tokio::process::Child::try_wait);
            match observed {
                Some(Ok(Some(exit))) => {
                    self.status = SessionStatus::Exited {
                        code: exit.code(),
                        signal: exit_signal(&exit),
                    };
                    self.child = None;
                }
                Some(Err(_)) => {
                    self.status = SessionStatus::Exited {
                        code: None,
                        signal: None,
                    };
                    self.child = None;
                }
                Some(Ok(None)) | None => {}
            }
        }
        self.status
    }

    /// Drains what the process produced since the last read (US-015 AC1).
    fn drain(&self) -> OutputBuffer {
        match self.buffer.lock() {
            Ok(mut buf) => buf.take(),
            Err(_) => OutputBuffer::default(),
        }
    }

    fn allocate_chunk(&mut self) -> u64 {
        self.next_chunk = self.next_chunk.saturating_add(1);
        self.next_chunk
    }

    fn writer(&mut self) -> Option<SessionWriter> {
        match &mut self.io {
            SessionIo::Pipe { stdin } => stdin.take().map(SessionWriter::Pipe),
            #[cfg(unix)]
            SessionIo::Pty { master } => Some(SessionWriter::Pty(Arc::clone(master))),
        }
    }

    /// Hands the pipe handle back to the session after a write.
    fn restore_pipe(&mut self, handle: tokio::process::ChildStdin) {
        match &mut self.io {
            SessionIo::Pipe { stdin } => *stdin = Some(handle),
            #[cfg(unix)]
            SessionIo::Pty { .. } => {}
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Synchronous on purpose: `Drop` cannot await, and a session dropped at
        // the end of the program must not leave an orphan behind.
        if let Some(pid) = self.pid {
            kill_group_blocking(pid);
        }
        if let Some(child) = self.child.as_mut() {
            // Reap the direct child here instead of leaving it to Tokio's
            // scheduler. Drop can run on the runtime's only thread, where an
            // async waiter would remain parked and expose a zombie as alive.
            let deadline = Instant::now() + Duration::from_secs(1);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Sessions of one Pyxis run, shared by both tools through the `ToolCtx`.
#[derive(Clone)]
pub struct ExecSessions {
    inner: Arc<Mutex<Store>>,
    idle_timeout: Duration,
}

impl Default for ExecSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of closed sessions whose cause stays addressable. Bounded: the
/// tombstones exist to answer the next call, not to become a log.
const CLOSED_MEMORY: usize = 32;

#[derive(Default)]
struct Store {
    sessions: HashMap<u64, Session>,
    /// Ids held between the cap check and the spawn (US-014 AC4): the slot is
    /// taken BEFORE a process exists, so the fifth call cannot race past it.
    reserved: HashSet<u64>,
    closed: VecDeque<(u64, ClosedCause)>,
    next_id: u64,
}

impl Store {
    fn close(&mut self, id: u64, cause: ClosedCause) -> Option<Session> {
        self.reserved.remove(&id);
        let session = self.sessions.remove(&id);
        if self.closed.len() >= CLOSED_MEMORY {
            self.closed.pop_front();
        }
        self.closed.push_back((id, cause));
        session
    }

    /// Names the state of an id that is not live. The three the story asks to
    /// tell apart (unknown, expired, already terminated) each get their own
    /// sentence, because the model's next move differs in each case.
    fn missing(&self, id: u64, idle: Duration) -> ToolError {
        let cause = self
            .closed
            .iter()
            .rev()
            .find(|(closed, _)| *closed == id)
            .map(|(_, cause)| *cause);
        let message = match cause {
            Some(ClosedCause::Idle) => format!(
                "shell session {id} expired after {}s without activity: open a new one \
                 with exec_command",
                idle.as_secs()
            ),
            Some(ClosedCause::Exited { code, signal }) => format!(
                "shell session {id} already ended ({}): open a new one with exec_command",
                describe_end(code, signal)
            ),
            Some(ClosedCause::Terminated) => {
                format!("shell session {id} was terminated: open a new one with exec_command")
            }
            Some(ClosedCause::Shutdown) => {
                format!("shell session {id} was closed when the run ended")
            }
            None if id <= self.next_id => format!(
                "shell session {id} is no longer tracked: it closed more than {CLOSED_MEMORY} \
                 sessions ago"
            ),
            None => format!("unknown shell session {id}: no session carries that id"),
        };
        ToolError::Rejected(message)
    }
}

impl ExecSessions {
    pub fn new() -> Self {
        Self::with_idle_timeout(IDLE_TIMEOUT)
    }

    /// Same store with another inactivity window. Exists so the closing
    /// behavior can be proven in a test without waiting five minutes.
    pub fn with_idle_timeout(idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Store::default())),
            idle_timeout,
        }
    }

    /// Number of live sessions (inspection, tests).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|s| s.sessions.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Closes every session and kills every process tree. Called when the Pyxis
    /// session ends; also what `Drop` falls back on.
    pub fn shutdown(&self) {
        let drained: Vec<Session> = match self.inner.lock() {
            Ok(mut store) => {
                let ids: Vec<u64> = store.sessions.keys().copied().collect();
                ids.into_iter()
                    .filter_map(|id| store.close(id, ClosedCause::Shutdown))
                    .collect()
            }
            Err(_) => Vec::new(),
        };
        // Dropped OUTSIDE the lock: each `Drop` kills a process tree, and
        // holding the mutex across that would serialize a watchdog onto it.
        drop(drained);
    }

    /// Takes a slot before anything is spawned (US-014 AC4). The id is already
    /// the session's: a refusal after this point releases it.
    fn reserve(&self) -> Result<u64, ToolError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| ToolError::Io("session store poisoned".to_string()))?;
        if store.sessions.len() + store.reserved.len() >= MAX_SESSIONS {
            return Err(ToolError::Rejected(format!(
                "too many open shell sessions ({MAX_SESSIONS}): terminate one \
                 (write_stdin with terminate) before opening another"
            )));
        }
        store.next_id += 1;
        let id = store.next_id;
        store.reserved.insert(id);
        Ok(id)
    }

    fn release(&self, id: u64) {
        if let Ok(mut store) = self.inner.lock() {
            store.reserved.remove(&id);
        }
    }

    fn commit(&self, id: u64, session: Session) {
        if let Ok(mut store) = self.inner.lock() {
            store.reserved.remove(&id);
            store.sessions.insert(id, session);
        }
    }

    /// Removes a session and returns it, so the caller drops it outside the lock.
    fn close(&self, id: u64, cause: ClosedCause) -> Option<Session> {
        self.inner.lock().ok()?.close(id, cause)
    }

    /// Runs `f` on a live session, refreshing its activity stamp.
    fn with_session<T>(&self, id: u64, f: impl FnOnce(&mut Session) -> T) -> Result<T, ToolError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| ToolError::Io("session store poisoned".to_string()))?;
        let idle = self.idle_timeout;
        let Some(session) = store.sessions.get_mut(&id) else {
            return Err(store.missing(id, idle));
        };
        session.last_activity = Instant::now();
        Ok(f(session))
    }

    /// Watchdog of one session: closes it once idle past the timeout.
    fn watch(&self, id: u64) {
        let weak = Arc::downgrade(&self.inner);
        let idle = self.idle_timeout;
        // A window shorter than the tick would never be observed: the watchdog
        // has to look at least as often as it is asked to close.
        let tick = WATCHDOG_TICK.min(idle);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(inner) = Weak::upgrade(&weak) else {
                    // The store is gone with the run: `Drop` already killed.
                    return;
                };
                let expired = {
                    let Ok(mut store) = inner.lock() else {
                        return;
                    };
                    match store.sessions.get(&id) {
                        None => return,
                        Some(session) if session.last_activity.elapsed() >= idle => {
                            store.close(id, ClosedCause::Idle)
                        }
                        Some(_) => None,
                    }
                };
                if let Some(session) = expired {
                    tracing::debug!(
                        target: "pyxis::tools",
                        session = id,
                        "shell session closed after inactivity"
                    );
                    drop(session);
                    return;
                }
            }
        });
    }
}

// ───────────────────────── exec_command ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecCommandInput {
    /// Baseline name: the models are trained on `cmd`.
    pub cmd: String,
    /// Working directory of the command; defaults to the workspace root.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Shell binary to launch; defaults to the session shell.
    #[serde(default)]
    pub shell: Option<String>,
    /// True allocates a PTY; false or absent uses plain pipes.
    #[serde(default)]
    pub tty: Option<bool>,
    /// How long to wait for output before handing back, in milliseconds.
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    /// Output budget of this call, in tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

pub struct ExecCommand;

#[async_trait]
impl Tool for ExecCommand {
    type Input = ExecCommandInput;

    fn name(&self) -> &str {
        "exec_command"
    }
    fn call_kind(&self, input: &serde_json::Value) -> agent_core::event::ToolCallKind {
        match input.get("cmd").and_then(serde_json::Value::as_str) {
            Some(command) => agent_core::event::ToolCallKind::Exec {
                command: command.to_string(),
                cwd: input
                    .get("workdir")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            },
            None => agent_core::event::ToolCallKind::Other,
        }
    }
    fn description(&self) -> String {
        format!(
            "Run a command in a PERSISTENT terminal session whose standard input \
             stays open, returning its output or a session_id for ongoing \
             interaction. Use it for anything that asks a question, waits for a \
             keypress or runs long; `bash` stays the right tool for a one-shot \
             command. `tty: true` allocates a real pseudo-terminal, which is what \
             a program checking isatty needs. Answer a prompt with write_stdin on \
             the session_id returned, and end a session with write_stdin \
             terminate. A session is closed after {}s of inactivity. Parameters: \
             cmd, workdir, shell, tty, yield_time_ms, max_output_tokens.",
            IDLE_TIMEOUT.as_secs()
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Shell command to execute." },
                "workdir": {
                    "type": ["string", "null"],
                    "description": "Working directory for the command. Defaults to the workspace root."
                },
                "shell": {
                    "type": ["string", "null"],
                    "description": "Shell binary to launch. Defaults to the session shell."
                },
                "tty": {
                    "type": ["boolean", "null"],
                    "description": "True allocates a PTY for the command; false or null uses plain pipes."
                },
                "yield_time_ms": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Wait before yielding output. Defaults to 10000 ms; effective range is 250-30000 ms."
                },
                "max_output_tokens": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped."
                }
            },
            "required": ["cmd", "workdir", "shell", "tty", "yield_time_ms", "max_output_tokens"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        true
    }
    fn returns_untrusted(&self) -> bool {
        true
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        EXEC_GUIDELINES
    }
    /// The session inherits the perimeter in force (US-014 AC3), and the three
    /// refusals of AC4 are decided here, before anything is spawned.
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_shell_payload(&input.cmd, ctx)?;
        resolve_workdir(input.workdir.as_deref(), ctx)?;
        resolve_shell(input.shell.as_deref())?;
        if input.tty == Some(true) && !cfg!(unix) {
            return Err(ValidationError::new(
                "tty sessions require a Unix host: rerun with tty false",
            ));
        }
        Ok(())
    }
    /// Same baseline as `bash`: the decision follows the command.
    fn permission(&self, input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        match crate::command::classify(&input.cmd) {
            crate::command::CommandClass::SideEffectFree(_) => PermissionDecision::Allow,
            crate::command::CommandClass::Argv(_) | crate::command::CommandClass::Opaque(_) => {
                PermissionDecision::Ask
            }
        }
    }
    fn approval_memo(&self, input: &Self::Input) -> crate::permission::ApprovalMemo {
        use crate::permission::ApprovalMemo;
        match crate::command::classify(&input.cmd) {
            crate::command::CommandClass::SideEffectFree(tokens)
            | crate::command::CommandClass::Argv(tokens) => ApprovalMemo::Key(tokens),
            crate::command::CommandClass::Opaque(reason) => ApprovalMemo::Refused(reason),
        }
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        let sessions = ctx.sessions.clone();
        // Order matters (AC4): everything that can refuse runs before the slot,
        // and the slot runs before the process.
        let workdir = resolve_workdir(input.workdir.as_deref(), ctx)?;
        let shell = resolve_shell(input.shell.as_deref())?;
        let tty = input.tty.unwrap_or(false);
        let id = sessions.reserve()?;

        let session = match spawn_session(&shell, &input.cmd, &workdir, tty, ctx) {
            Ok(session) => session,
            Err(e) => {
                sessions.release(id);
                return Err(e);
            }
        };
        sessions.commit(id, session);
        sessions.watch(id);

        let window = yield_window(input.yield_time_ms, DEFAULT_EXEC_YIELD);
        let collected = collect(&sessions, id, window, ctx).await?;
        Ok(finish(
            &sessions,
            id,
            collected,
            started,
            input.max_output_tokens,
            false,
        ))
    }
}

// ───────────────────────── write_stdin ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteStdinInput {
    pub session_id: u64,
    /// Bytes written to the session's standard input, verbatim. Empty polls the
    /// session without writing. Add the newline yourself when the program waits
    /// for a line.
    #[serde(default)]
    pub chars: String,
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Ends the session: the process group is signalled and reaped.
    #[serde(default)]
    pub terminate: Option<bool>,
}

impl WriteStdinInput {
    fn terminates(&self) -> bool {
        self.terminate.unwrap_or(false)
    }
}

pub struct WriteStdin;

#[async_trait]
impl Tool for WriteStdin {
    type Input = WriteStdinInput;

    fn name(&self) -> &str {
        "write_stdin"
    }
    fn description(&self) -> String {
        "Write to the standard input of a session opened by exec_command, then \
         return the output produced within the wait. The text is sent verbatim: \
         end it with a newline when the program waits for a line. An empty chars \
         polls the session and returns only what it produced since the previous \
         chunk; terminate ends the session and its process group. Parameters: \
         session_id, chars, yield_time_ms, max_output_tokens, terminate."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "integer", "minimum": 1, "description": "Session returned by exec_command." },
                "chars": {
                    "type": "string",
                    "description": "Bytes written to standard input, verbatim. Empty polls without writing."
                },
                "yield_time_ms": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Wait before yielding output. Writes default to 250 ms, empty polls to 5000 ms; effective range is 250-30000 ms."
                },
                "max_output_tokens": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped."
                },
                "terminate": {
                    "type": ["boolean", "null"],
                    "description": "True ends the session and its process group. Requires an empty chars."
                }
            },
            "required": ["session_id", "chars", "yield_time_ms", "max_output_tokens", "terminate"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        true
    }
    fn returns_untrusted(&self) -> bool {
        true
    }
    /// What is written to a shell's stdin IS a command. Same guardrail as
    /// `exec_command`, otherwise the session would be the documented way around
    /// the protected subpaths. A poll writes nothing, so it has nothing to guard.
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.terminates() && !input.chars.is_empty() {
            return Err(ValidationError::new(
                "terminate takes no input: send the bytes first, then terminate",
            ));
        }
        if input.chars.is_empty() {
            return Ok(());
        }
        validate_shell_payload(&input.chars, ctx)
    }
    /// Fail-closed on what is WRITTEN: stdin feeds a program whose state we do
    /// not know, so it is never auto-approved. Reading the session back, and
    /// ending a process Pyxis itself started, add no reach.
    fn permission(&self, input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        if input.chars.is_empty() {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Ask
        }
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        let sessions = ctx.sessions.clone();
        let id = input.session_id;
        let terminating = input.terminates();

        if !input.chars.is_empty() {
            write_to_session(&sessions, id, input.chars.as_bytes()).await?;
        } else {
            // A poll must still name the state of a session that is gone
            // (US-015 AC4) rather than report an empty read.
            sessions.with_session(id, |_| ())?;
        }

        if terminating {
            terminate_session(&sessions, id).await?;
        }

        let window = yield_window(
            input.yield_time_ms,
            if terminating {
                MIN_YIELD
            } else if input.chars.is_empty() {
                DEFAULT_POLL_YIELD
            } else {
                DEFAULT_WRITE_YIELD
            },
        );
        let collected = collect(&sessions, id, window, ctx).await?;
        Ok(finish(
            &sessions,
            id,
            collected,
            started,
            input.max_output_tokens,
            terminating,
        ))
    }
}

const EXEC_GUIDELINES: &[&str] = &[
    "exec_command / write_stdin: use them ONLY for a command that waits for \
     input or runs long. Everything else goes through bash, which is one-shot \
     and leaves no session behind. Always answer a prompt with write_stdin on \
     the session_id returned, never by re-running the command; poll a running \
     session with an empty chars, and release it with terminate once done.",
];

/// Shared guardrail of both tools: bounds, then the protected subpaths.
fn validate_shell_payload(payload: &str, ctx: &ToolCtx) -> Result<(), ValidationError> {
    if payload.is_empty() {
        return Err(ValidationError::new("empty payload"));
    }
    if payload.len() > MAX_COMMAND_BYTES {
        return Err(ValidationError::new(format!(
            "payload too large: {} bytes > {MAX_COMMAND_BYTES}",
            payload.len()
        )));
    }
    crate::path::guard_command_paths(&ctx.sandbox, &ctx.workspace, payload)
}

/// Resolves the working directory of the command (US-014 AC1/AC4): confined to
/// the workspace, and existing, both decided before the process.
fn resolve_workdir(workdir: Option<&str>, ctx: &ToolCtx) -> Result<PathBuf, ValidationError> {
    let Some(requested) = workdir.filter(|w| !w.is_empty()) else {
        return Ok(ctx.workspace.clone());
    };
    let target = crate::path::confine(&ctx.workspace, requested)
        .map_err(|e| ValidationError::new(e.to_string()))?;
    if !target.is_dir() {
        return Err(ValidationError::new(format!(
            "workdir `{requested}` does not exist or is not a directory"
        )));
    }
    Ok(target)
}

/// Resolves the shell of the command: the session one by default, the requested
/// one when it is a POSIX shell Pyxis runs, a refusal otherwise (US-014 AC4).
fn resolve_shell(requested: Option<&str>) -> Result<crate::shell::ShellChoice, ValidationError> {
    match requested.filter(|s| !s.is_empty()) {
        None => Ok(crate::shell::resolve()),
        Some(name) => crate::shell::select_requested(name).map_err(ValidationError::new),
    }
}

/// Clamps a requested wait to the baseline range, defaulting per call shape.
fn yield_window(requested: Option<u64>, default: Duration) -> Duration {
    match requested {
        Some(ms) => Duration::from_millis(ms).clamp(MIN_YIELD, MAX_YIELD),
        None => default.clamp(MIN_YIELD, MAX_YIELD),
    }
}

/// Spawns the session process: same hardening as `bash`, plus an OPEN standard
/// input and, on request, a real terminal.
fn spawn_session(
    shell: &crate::shell::ShellChoice,
    command: &str,
    workdir: &std::path::Path,
    tty: bool,
    ctx: &ToolCtx,
) -> Result<Session, ToolError> {
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg("-NoProfile").arg("-Command").arg(command);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg("-c").arg(command);
        cmd
    };
    cmd.current_dir(workdir).kill_on_drop(true);
    for (key, value) in SESSION_ENV {
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    if let Some(harden) = &ctx.harden {
        harden(&mut cmd);
    }

    let buffer = Arc::new(Mutex::new(OutputBuffer::default()));
    #[cfg(unix)]
    if tty {
        let session = crate::pty::spawn(&mut cmd)
            .map_err(|e| ToolError::Io(format!("terminal session launch: {e}")))?;
        let pid = session.child.id();
        pump_pty(Arc::clone(&session.master), Arc::clone(&buffer));
        return Ok(Session {
            pid,
            child: Some(session.child),
            io: SessionIo::Pty {
                master: session.master,
            },
            buffer,
            status: SessionStatus::Running,
            last_activity: Instant::now(),
            command: command.to_string(),
            next_chunk: 0,
        });
    }
    #[cfg(not(unix))]
    if tty {
        return Err(ToolError::Rejected(
            "tty sessions require a Unix host".to_string(),
        ));
    }

    // Pipe session: the process group is its own, so terminating it never
    // reaches Pyxis.
    #[cfg(not(windows))]
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Io(format!("shell session launch: {e}")))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    pump_pipe(child.stdout.take(), Arc::clone(&buffer));
    pump_pipe(child.stderr.take(), Arc::clone(&buffer));
    Ok(Session {
        pid,
        child: Some(child),
        io: SessionIo::Pipe { stdin },
        buffer,
        status: SessionStatus::Running,
        last_activity: Instant::now(),
        command: command.to_string(),
        next_chunk: 0,
    })
}

/// Reads a pipe into the session buffer until EOF.
fn pump_pipe<R>(reader: Option<R>, buffer: Arc<Mutex<OutputBuffer>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else {
        return;
    };
    tokio::spawn(async move {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut out) = buffer.lock() {
                        out.push(&buf[..n]);
                    }
                }
            }
        }
    });
}

/// Reads the terminal into the session buffer until the last slave closes.
#[cfg(unix)]
fn pump_pty(master: Arc<crate::pty::PtyMaster>, buffer: Arc<Mutex<OutputBuffer>>) {
    tokio::spawn(async move {
        let mut buf = [0_u8; 8192];
        loop {
            match master.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut out) = buffer.lock() {
                        out.push(&buf[..n]);
                    }
                }
            }
        }
    });
}

/// Writes to a session's input, whatever backs it.
async fn write_to_session(
    sessions: &ExecSessions,
    id: u64,
    payload: &[u8],
) -> Result<(), ToolError> {
    // The handle is taken out of the store for the duration, so the mutex is
    // never held across an await.
    let writer = sessions.with_session(id, |session| match session.status() {
        SessionStatus::Exited { code, signal } => Err(ToolError::Rejected(format!(
            "shell session {id} already ended ({}): open a new one with exec_command",
            describe_end(code, signal)
        ))),
        SessionStatus::Running => session.writer().ok_or_else(|| {
            ToolError::Rejected(format!("shell session {id} has no open standard input"))
        }),
    })??;

    let outcome = match writer {
        SessionWriter::Pipe(mut stdin) => {
            let written = async {
                stdin.write_all(payload).await?;
                stdin.flush().await
            }
            .await;
            // Handed back whatever happened: a failed write must not turn the
            // session into one that can never be written to again.
            let _ = sessions.with_session(id, |session| session.restore_pipe(stdin));
            written
        }
        #[cfg(unix)]
        SessionWriter::Pty(master) => master.write_all(payload).await,
    };
    outcome.map_err(|e| ToolError::Io(format!("shell session {id} stdin: {e}")))
}

/// Ends a session's process group within the story's budget (US-015 AC2):
/// `SIGTERM`, a grace period, then `SIGKILL`, the whole under 2 seconds.
async fn terminate_session(sessions: &ExecSessions, id: u64) -> Result<(), ToolError> {
    let (pid, status) = sessions.with_session(id, |session| (session.pid, session.status()))?;
    if matches!(status, SessionStatus::Exited { .. }) {
        return Ok(());
    }
    let Some(pid) = pid else {
        return Ok(());
    };

    signal_group(pid, Signal::Term);
    if wait_for_exit(sessions, id, TERMINATE_GRACE).await? {
        return Ok(());
    }
    signal_group(pid, Signal::Kill);
    wait_for_exit(sessions, id, KILL_GRACE).await?;
    Ok(())
}

/// Polls the session until its process is gone or `budget` expires.
async fn wait_for_exit(
    sessions: &ExecSessions,
    id: u64,
    budget: Duration,
) -> Result<bool, ToolError> {
    let deadline = Instant::now() + budget;
    loop {
        let status = sessions.with_session(id, Session::status)?;
        if matches!(status, SessionStatus::Exited { .. }) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

/// What a yield produced.
struct Collected {
    buffer: OutputBuffer,
    status: SessionStatus,
    chunk_id: u64,
}

/// Waits for output, publishing it as it comes through the fragment channel
/// every other tool already uses. Returns early once the process exits: there is
/// nothing more to wait for.
async fn collect(
    sessions: &ExecSessions,
    id: u64,
    wait: Duration,
    ctx: &ToolCtx,
) -> Result<Collected, ToolError> {
    let deadline = Instant::now() + wait;
    let mut accumulated = OutputBuffer::default();
    loop {
        let (chunk, status) =
            sessions.with_session(id, |session| (session.drain(), session.status()))?;
        emit(ctx, &chunk, &mut accumulated);
        if let SessionStatus::Exited { .. } = status {
            // One last drain, after letting the reader task run: `try_wait` can
            // report the exit before the pump has consumed what the process
            // wrote on its way out, and that tail is usually the answer.
            tokio::time::sleep(POLL).await;
            let (tail, status) =
                sessions.with_session(id, |session| (session.drain(), session.status()))?;
            emit(ctx, &tail, &mut accumulated);
            let chunk_id = sessions.with_session(id, Session::allocate_chunk)?;
            return Ok(Collected {
                buffer: accumulated,
                status,
                chunk_id,
            });
        }
        if Instant::now() >= deadline {
            let chunk_id = sessions.with_session(id, Session::allocate_chunk)?;
            return Ok(Collected {
                buffer: accumulated,
                status: SessionStatus::Running,
                chunk_id,
            });
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

fn emit(ctx: &ToolCtx, chunk: &OutputBuffer, accumulated: &mut OutputBuffer) {
    if !chunk.bytes.is_empty() {
        // A session multiplexes both streams onto one PTY or one pipe pair and
        // no longer knows which produced what, so everything is reported as
        // stdout rather than guessed.
        ctx.emit_output(
            agent_core::event::OutputStream::Stdout,
            chunk.bytes.clone(),
        );
    }
    accumulated.absorb(chunk);
}

/// Shapes the baseline result and closes the session when its process is gone:
/// an exited session must not stay in the store waiting for the watchdog.
fn finish(
    sessions: &ExecSessions,
    id: u64,
    collected: Collected,
    started: Instant,
    max_output_tokens: Option<u64>,
    terminated: bool,
) -> ToolOutput {
    let Collected {
        mut buffer,
        status,
        chunk_id,
    } = collected;
    // The model budget is applied LAST, so what it drops is counted with the
    // rest (US-015 AC3).
    buffer.trim_to(output_budget(max_output_tokens));
    let original_tokens = approx_tokens(buffer.produced);
    let mut body = String::new();
    if buffer.omitted > 0 {
        // Same marker as the baseline (`format_output_omission_marker`).
        body.push_str(&format!("... {} bytes omitted ...\n", buffer.omitted));
    }
    body.push_str(&String::from_utf8_lossy(&buffer.bytes));

    let (exit_code, signal, session_id) = match status {
        SessionStatus::Exited { code, signal } => {
            let closed = sessions.close(
                id,
                if terminated {
                    ClosedCause::Terminated
                } else {
                    ClosedCause::Exited { code, signal }
                },
            );
            // The command is kept on the session for the trace below, which
            // stays at `trace` level: a command line can carry a token, and the
            // observability policy keeps call content out of `debug`.
            let command = closed
                .as_ref()
                .map(|s| s.command.clone())
                .unwrap_or_default();
            drop(closed);
            if let Some(code) = code.filter(|c| *c != 0) {
                tracing::debug!(
                    target: "pyxis::tools",
                    session = id,
                    exit_code = code,
                    "shell session exited with a failure"
                );
                tracing::trace!(
                    target: "pyxis::tools",
                    session = id,
                    %command,
                    "shell session command"
                );
            }
            (code, signal, None)
        }
        SessionStatus::Running => (None, None, Some(id)),
    };

    let wall_time = started.elapsed();
    let mut sections = vec![
        format!("Chunk ID: {chunk_id}"),
        format!("Wall time: {:.4} seconds", wall_time.as_secs_f64()),
    ];
    if let Some(code) = exit_code {
        sections.push(format!("Process exited with code {code}"));
    }
    if let Some(signal) = signal {
        sections.push(format!("Process terminated by signal {signal}"));
    }
    if let Some(session_id) = session_id {
        sections.push(format!("Process running with session ID {session_id}"));
    }
    sections.push(format!("Original token count: {original_tokens}"));
    sections.push("Output:".to_string());
    sections.push(body.clone());
    let text = sections.join("\n");

    let mut structured = serde_json::json!({
        "chunk_id": chunk_id.to_string(),
        "wall_time_seconds": wall_time.as_secs_f64(),
        "original_token_count": original_tokens,
        "output": body,
    });
    if let Some(map) = structured.as_object_mut() {
        if let Some(code) = exit_code {
            map.insert("exit_code".to_string(), code.into());
        }
        if let Some(signal) = signal {
            map.insert("signal".to_string(), signal.into());
        }
        if let Some(session_id) = session_id {
            map.insert("session_id".to_string(), session_id.into());
        }
        if buffer.omitted > 0 {
            map.insert("output_omitted_bytes".to_string(), buffer.omitted.into());
        }
    }

    // A terminated session ended on request: that is an outcome, not a failure.
    let failed = !terminated && (exit_code.is_some_and(|c| c != 0) || signal.is_some());
    let output = if failed {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    };
    output
        .with_structured_content(structured)
        .with_execution(ToolExecution {
            exit_code,
            signal,
            timed_out: false,
            cancelled: terminated,
            stderr_tail: None,
        })
}

/// Model-facing byte budget of one chunk, from the baseline token budget.
fn output_budget(max_output_tokens: Option<u64>) -> usize {
    let tokens = max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let bytes = tokens.saturating_mul(APPROX_BYTES_PER_TOKEN);
    usize::try_from(bytes)
        .unwrap_or(MAX_CHUNK_BYTES)
        .min(MAX_CHUNK_BYTES)
}

/// Same 4 bytes per token approximation as the baseline.
fn approx_tokens(bytes: u64) -> u64 {
    bytes.saturating_add(APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN
}

fn describe_end(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("terminated by signal {signal}"),
        (None, None) => "terminated".to_string(),
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signals a process GROUP. The session leads its own group (`process_group` for
/// a pipe, `setsid` for a PTY), so this never reaches Pyxis itself.
fn signal_group(pid: u32, signal: Signal) {
    #[cfg(unix)]
    {
        let Ok(raw) = i32::try_from(pid) else {
            return;
        };
        let signal = match signal {
            Signal::Term => nix::sys::signal::Signal::SIGTERM,
            Signal::Kill => nix::sys::signal::Signal::SIGKILL,
        };
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal);
    }
    #[cfg(windows)]
    {
        let _ = signal;
        let _ = std::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Kills a process group without awaiting. Used by `Drop`, which cannot.
fn kill_group_blocking(pid: u32) {
    signal_group(pid, Signal::Kill);
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    fn exec(cmd: &str, yield_time_ms: Option<u64>) -> ExecCommandInput {
        ExecCommandInput {
            cmd: cmd.to_string(),
            workdir: None,
            shell: None,
            tty: None,
            yield_time_ms,
            max_output_tokens: None,
        }
    }

    fn poll(session_id: u64, yield_time_ms: Option<u64>) -> WriteStdinInput {
        WriteStdinInput {
            session_id,
            chars: String::new(),
            yield_time_ms,
            max_output_tokens: None,
            terminate: None,
        }
    }

    fn field<'a>(output: &'a ToolOutput, key: &str) -> Option<&'a serde_json::Value> {
        output.structured_content.as_ref()?.get(key)
    }

    // US-014 AC1: the result carries output, exit code, session, chunk and
    // elapsed time in the baseline shape.
    #[tokio::test]
    async fn a_finished_command_reports_the_baseline_wire() {
        let ctx = ctx();
        let out = ExecCommand
            .call(exec("echo hello", Some(5_000)), &ctx)
            .await
            .expect("a simple command must run");
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("Chunk ID: 1"), "{}", out.content);
        assert!(out.content.contains("Wall time: "), "{}", out.content);
        assert!(
            out.content.contains("Process exited with code 0"),
            "{}",
            out.content
        );
        assert!(out.content.contains("\nOutput:\n"), "{}", out.content);
        assert_eq!(field(&out, "exit_code"), Some(&serde_json::json!(0)));
        assert_eq!(field(&out, "chunk_id"), Some(&serde_json::json!("1")));
        assert!(field(&out, "wall_time_seconds").is_some());
        assert!(field(&out, "session_id").is_none(), "the session is over");
        assert!(
            ctx.sessions.is_empty(),
            "an exited session must not stay in the store"
        );
    }

    #[tokio::test]
    async fn a_command_waiting_for_input_no_longer_dies_on_a_closed_stdin() {
        let ctx = ctx();
        // `read` on a closed stdin returns immediately; here it must WAIT.
        let opened = ExecCommand
            .call(exec("read line; echo \"got:$line\"", Some(300)), &ctx)
            .await
            .expect("the session must open");
        assert_eq!(field(&opened, "session_id"), Some(&serde_json::json!(1)));
        assert_eq!(ctx.sessions.len(), 1);

        let answered = WriteStdin
            .call(
                WriteStdinInput {
                    session_id: 1,
                    chars: "pyxis\n".to_string(),
                    yield_time_ms: Some(5_000),
                    max_output_tokens: None,
                    terminate: None,
                },
                &ctx,
            )
            .await
            .expect("the write must succeed");
        assert!(
            answered.content.contains("got:pyxis"),
            "the program must have read stdin: {}",
            answered.content
        );
        assert!(ctx.sessions.is_empty(), "the session must close on exit");
    }

    // US-014 AC1: a valid workdir and a valid shell are HONORED, not merely
    // accepted; the command runs where and with what it was told to.
    #[tokio::test]
    async fn a_valid_workdir_and_shell_are_the_ones_used() {
        let root = std::env::temp_dir().join(format!("pyxis-exec-{}", std::process::id()));
        std::fs::create_dir_all(root.join("nested")).expect("the fixture must exist");
        let ctx = ToolCtx::new(&root);
        let out = ExecCommand
            .call(
                ExecCommandInput {
                    cmd: "pwd; echo \"shell=$0\"".to_string(),
                    workdir: Some("nested".to_string()),
                    shell: Some("/bin/sh".to_string()),
                    tty: Some(false),
                    yield_time_ms: Some(3_000),
                    max_output_tokens: None,
                },
                &ctx,
            )
            .await
            .expect("the command must run");
        assert!(out.content.contains("nested"), "{}", out.content);
        assert!(out.content.contains("shell=/bin/sh"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&root);
    }

    // US-014 AC1/AC2: a PTY session makes a program believe it talks to a
    // terminal, and what is written reaches that session and only that one.
    #[tokio::test]
    async fn a_tty_session_gives_the_program_a_terminal() {
        let ctx = ctx();
        let out = ExecCommand
            .call(
                ExecCommandInput {
                    cmd: "test -t 0 && echo interactive || echo piped".to_string(),
                    workdir: None,
                    shell: None,
                    tty: Some(true),
                    yield_time_ms: Some(3_000),
                    max_output_tokens: None,
                },
                &ctx,
            )
            .await
            .expect("the pty session must open");
        assert!(out.content.contains("interactive"), "{}", out.content);

        let piped = ExecCommand
            .call(
                ExecCommandInput {
                    cmd: "test -t 0 && echo interactive || echo piped".to_string(),
                    workdir: None,
                    shell: None,
                    tty: Some(false),
                    yield_time_ms: Some(3_000),
                    max_output_tokens: None,
                },
                &ctx,
            )
            .await
            .expect("the pipe session must open");
        assert!(piped.content.contains("piped"), "{}", piped.content);
    }

    // US-014 AC2: two live sessions, and stdin reaches exactly one of them.
    #[tokio::test]
    async fn a_write_reaches_only_its_own_session() {
        let ctx = ctx();
        for _ in 0..2 {
            ExecCommand
                .call(
                    ExecCommandInput {
                        cmd: "read line; echo \"got:$line\"".to_string(),
                        workdir: None,
                        shell: None,
                        tty: Some(true),
                        yield_time_ms: Some(300),
                        max_output_tokens: None,
                    },
                    &ctx,
                )
                .await
                .expect("the session must open");
        }
        assert_eq!(ctx.sessions.len(), 2);

        let answered = WriteStdin
            .call(
                WriteStdinInput {
                    session_id: 2,
                    chars: "second\n".to_string(),
                    yield_time_ms: Some(3_000),
                    max_output_tokens: None,
                    terminate: None,
                },
                &ctx,
            )
            .await
            .expect("the write must succeed");
        assert!(answered.content.contains("got:second"), "{answered:?}");

        // The other session never saw the bytes: it is still waiting.
        let other = WriteStdin
            .call(poll(1, Some(300)), &ctx)
            .await
            .expect("the poll must succeed");
        assert!(!other.content.contains("got:"), "{}", other.content);
        assert_eq!(field(&other, "session_id"), Some(&serde_json::json!(1)));
        ctx.sessions.shutdown();
    }

    // US-015 AC1: a poll returns ONLY what came after the previous chunk, and
    // the chunk identifier grows.
    #[tokio::test]
    async fn a_poll_returns_only_what_came_after_the_previous_chunk() {
        let ctx = ctx();
        let first = ExecCommand
            .call(
                exec("echo one; sleep 0.5; echo two; sleep 30", Some(250)),
                &ctx,
            )
            .await
            .expect("the session must open");
        assert!(first.content.contains("one"), "{}", first.content);
        assert!(!first.content.contains("two"), "{}", first.content);
        assert_eq!(field(&first, "chunk_id"), Some(&serde_json::json!("1")));

        let second = WriteStdin
            .call(poll(1, Some(1_000)), &ctx)
            .await
            .expect("the poll must succeed");
        assert!(second.content.contains("two"), "{}", second.content);
        assert!(
            !second.content.contains("one"),
            "an already read chunk must not come back: {}",
            second.content
        );
        assert_eq!(field(&second, "chunk_id"), Some(&serde_json::json!("2")));
        ctx.sessions.shutdown();
    }

    // US-015 AC2: the session survives a yield that expires, and `terminate`
    // brings the process group to a terminal state well under two seconds.
    #[tokio::test]
    async fn a_yield_that_expires_keeps_the_session_and_terminate_ends_it() {
        let ctx = ctx();
        let opened = ExecCommand
            .call(
                ExecCommandInput {
                    cmd: "sleep 30".to_string(),
                    workdir: None,
                    shell: None,
                    tty: Some(true),
                    yield_time_ms: Some(300),
                    max_output_tokens: None,
                },
                &ctx,
            )
            .await
            .expect("the session must open");
        assert_eq!(field(&opened, "session_id"), Some(&serde_json::json!(1)));
        assert_eq!(ctx.sessions.len(), 1, "the session outlives the yield");
        let pid = ctx
            .sessions
            .with_session(1, |s| s.pid)
            .expect("the session must exist")
            .expect("the process must have a pid");

        let started = Instant::now();
        let ended = WriteStdin
            .call(
                WriteStdinInput {
                    session_id: 1,
                    chars: String::new(),
                    yield_time_ms: Some(250),
                    max_output_tokens: None,
                    terminate: Some(true),
                },
                &ctx,
            )
            .await
            .expect("the termination must succeed");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the process group must reach a terminal state under 2s"
        );
        assert!(!ended.is_error, "a termination on request is not a failure");
        assert!(ctx.sessions.is_empty(), "the session is gone");
        assert!(!is_alive(pid), "no process may survive the termination");
    }

    // US-015 AC3: what the cap drops is counted EXACTLY, and the memory of the
    // session never follows the size of the output.
    #[tokio::test]
    async fn a_huge_output_is_truncated_with_an_exact_omitted_count() {
        let ctx = ctx();
        // 12 MiB, past the per-result ceiling of the PRD.
        let out = ExecCommand
            .call(
                exec(
                    "head -c 12582912 /dev/zero | tr '\\0' 'x'; echo END",
                    Some(20_000),
                ),
                &ctx,
            )
            .await
            .expect("the command must run");
        let produced = 12_582_912_u64 + 4; // the payload plus "END\n"
        let omitted = field(&out, "output_omitted_bytes")
            .and_then(serde_json::Value::as_u64)
            .expect("a truncated output must report what it dropped");
        let kept = out
            .structured_content
            .as_ref()
            .and_then(|s| s.get("output"))
            .and_then(serde_json::Value::as_str)
            .map(|o| o.split_once('\n').map(|(_, rest)| rest.len()).unwrap_or(0))
            .expect("the output is reported");
        assert_eq!(
            omitted + kept as u64,
            produced,
            "omitted + kept must account for every byte"
        );
        assert!(kept <= MAX_CHUNK_BYTES, "the kept tail stays bounded");
        assert_eq!(
            field(&out, "original_token_count"),
            Some(&serde_json::json!(approx_tokens(produced))),
        );
        assert!(out.content.contains("bytes omitted"), "{}", out.content);
    }

    // US-015 AC4: unknown, expired and already terminated are three different
    // answers, and none of them writes to another process.
    #[tokio::test]
    async fn the_three_dead_session_states_are_told_apart() {
        let mut ctx = ctx();
        ctx.sessions = ExecSessions::with_idle_timeout(Duration::from_millis(150));

        let unknown = WriteStdin
            .call(poll(999, Some(250)), &ctx)
            .await
            .expect_err("an unknown session must be refused");
        assert!(
            unknown.to_string().contains("unknown shell session"),
            "{unknown}"
        );

        // Terminated: the process ended on its own.
        ExecCommand
            .call(exec("echo done", Some(3_000)), &ctx)
            .await
            .expect("the command must run");
        let ended = WriteStdin
            .call(poll(1, Some(250)), &ctx)
            .await
            .expect_err("an ended session must be refused");
        assert!(ended.to_string().contains("already ended"), "{ended}");

        // Expired: the watchdog closed an idle session.
        ExecCommand
            .call(exec("sleep 30", Some(250)), &ctx)
            .await
            .expect("the session must open");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let expired = WriteStdin
            .call(poll(2, Some(250)), &ctx)
            .await
            .expect_err("an expired session must be refused");
        assert!(expired.to_string().contains("expired"), "{expired}");
    }

    // US-014 AC4: the three refusals land BEFORE any process exists.
    #[tokio::test]
    async fn the_refusals_precede_the_spawn() {
        let ctx = ctx();
        let missing_workdir = ExecCommand
            .validate_input(
                &ExecCommandInput {
                    cmd: "echo hi".to_string(),
                    workdir: Some("nope-does-not-exist".to_string()),
                    shell: None,
                    tty: None,
                    yield_time_ms: None,
                    max_output_tokens: None,
                },
                &ctx,
            )
            .expect_err("an absent workdir must be refused");
        assert!(missing_workdir.to_string().contains("workdir"));

        let refused_shell = ExecCommand
            .validate_input(
                &ExecCommandInput {
                    cmd: "echo hi".to_string(),
                    workdir: None,
                    shell: Some("/usr/bin/fish".to_string()),
                    tty: None,
                    yield_time_ms: None,
                    max_output_tokens: None,
                },
                &ctx,
            )
            .expect_err("a non-POSIX shell must be refused");
        assert!(refused_shell.to_string().contains("fish"));

        for _ in 0..MAX_SESSIONS {
            ExecCommand
                .call(exec("sleep 30", Some(250)), &ctx)
                .await
                .expect("opening a session below the cap must succeed");
        }
        let over = ExecCommand
            .call(exec("sleep 30", Some(250)), &ctx)
            .await
            .expect_err("past the cap the session must be refused");
        assert!(over.to_string().contains("too many open"), "{over}");
        assert_eq!(
            ctx.sessions.len(),
            MAX_SESSIONS,
            "the refused call left nothing behind"
        );
        ctx.sessions.shutdown();
        assert!(ctx.sessions.is_empty());
    }

    // The output travels AS IT COMES on the fragment channel every other tool
    // already uses, not only in the final result.
    #[tokio::test]
    async fn session_output_travels_on_the_fragment_channel() {
        let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let chunks = Arc::clone(&chunks);
            Arc::new(
                move |_stream: agent_core::event::OutputStream, chunk: Vec<u8>| {
                    if let Ok(mut c) = chunks.lock() {
                        c.push(String::from_utf8_lossy(&chunk).into_owned());
                    }
                },
            )
        };
        let ctx = ctx().for_call(
            agent_core::message::ToolCallId::from("c1".to_string()),
            sink,
        );
        let out = ExecCommand
            .call(exec("echo un; sleep 0.2; echo deux", Some(5_000)), &ctx)
            .await
            .expect("the command must run");

        let seen = chunks.lock().unwrap().concat();
        assert!(
            seen.contains("un") && seen.contains("deux"),
            "les fragments doivent porter la sortie: {seen:?}"
        );
        assert!(
            chunks.lock().unwrap().len() >= 2,
            "la sortie doit arriver au fil de l'eau, pas en un bloc final: {:?}",
            chunks.lock().unwrap()
        );
        assert!(out.content.contains("deux"), "{}", out.content);
    }

    #[tokio::test]
    async fn shutdown_terminates_the_children() {
        let ctx = ctx();
        ExecCommand
            .call(exec("sleep 30", Some(250)), &ctx)
            .await
            .expect("the session must open");
        let pid = ctx
            .sessions
            .with_session(1, |s| s.pid)
            .expect("the session must exist")
            .expect("the process must have a pid");
        ctx.sessions.shutdown();
        assert!(ctx.sessions.is_empty());
        // The kill is a signal, not a promise of immediacy: we let the OS reap.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !is_alive(pid),
            "the process tree must be dead after shutdown"
        );
    }

    #[test]
    fn stdin_goes_through_the_protected_subpath_guardrail() {
        let ctx = ctx();
        // A session must not become the way around `guard_command_paths`.
        let err = WriteStdin
            .validate_input(
                &WriteStdinInput {
                    session_id: 1,
                    chars: "echo pwned > .git/hooks/pre-commit\n".to_string(),
                    yield_time_ms: None,
                    max_output_tokens: None,
                    terminate: None,
                },
                &ctx,
            )
            .expect_err("a protected subpath must be refused on stdin too");
        assert!(err.to_string().contains(".git"), "{err}");
    }

    // AC3: past the inactivity window the session is closed and its process
    // killed, WITHOUT waiting for another call to notice.
    #[tokio::test]
    async fn an_idle_session_is_closed_and_its_process_killed() {
        let mut ctx = ctx();
        ctx.sessions = ExecSessions::with_idle_timeout(Duration::from_millis(150));
        ExecCommand
            .call(exec("sleep 30", Some(250)), &ctx)
            .await
            .expect("the session must open");
        let pid = ctx
            .sessions
            .with_session(1, |s| s.pid)
            .expect("the session must exist")
            .expect("the process must have a pid");

        // Nothing touches the session: only the watchdog can close it.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            ctx.sessions.is_empty(),
            "the idle session must have been closed on its own"
        );
        assert!(!is_alive(pid), "no orphan process may survive the closing");
    }

    #[test]
    fn the_yield_window_follows_the_baseline_range() {
        assert_eq!(yield_window(None, DEFAULT_EXEC_YIELD), DEFAULT_EXEC_YIELD);
        assert_eq!(yield_window(None, DEFAULT_WRITE_YIELD), MIN_YIELD);
        assert_eq!(yield_window(Some(10), DEFAULT_EXEC_YIELD), MIN_YIELD);
        assert_eq!(yield_window(Some(u64::MAX), DEFAULT_EXEC_YIELD), MAX_YIELD);
    }

    #[test]
    fn the_output_budget_never_exceeds_the_chunk_cap() {
        assert_eq!(output_budget(None), MAX_CHUNK_BYTES);
        assert_eq!(output_budget(Some(10)), 40);
        assert_eq!(output_budget(Some(u64::MAX)), MAX_CHUNK_BYTES);
    }

    fn is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
