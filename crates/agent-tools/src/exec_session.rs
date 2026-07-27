//! Persistent shell sessions (US-012): `exec_command` opens one and runs a
//! command in it, `write_stdin` feeds its standard input. The split is Codex's
//! (`codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` and
//! `write_stdin.rs`): two tools, each idempotent to describe, instead of one
//! tool with a mode.
//!
//! **Pipes, not a PTY** (the PRD left the question open). A pipe closes the AC2
//! gap, which is the input stream being closed immediately: `bash` runs with
//! `stdin(null)`, so anything waiting for input dies instantly. A PTY would add
//! terminal emulation and a dependency to make programs *believe* they are
//! interactive; nothing in the acceptance criteria asks for that, so it stays
//! out until a command actually needs it.
//!
//! **The confinement covers stdin too.** A session is a shell: without the same
//! guardrail on what is written to it, opening `sh` then typing the command
//! would bypass the protected-subpath check that `bash` goes through
//! (`guard_command_paths`). Both tools therefore run it, on the command and on
//! every stdin payload.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_COMMAND_BYTES, Tool, ToolCtx, ToolOutput, truncate_tail};

/// Concurrent sessions cap. A session holds a process and two reader tasks;
/// past a handful the model is juggling, not working.
pub const MAX_SESSIONS: usize = 4;
/// Inactivity past which a session is closed and its process tree killed
/// (AC3). Counted from the last read or write on the session.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Watchdog tick. Fine enough that a forgotten session does not survive long,
/// coarse enough to cost nothing.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// Default wait after a command or a stdin write, when the model asks for none.
const DEFAULT_WAIT: Duration = Duration::from_millis(2_000);
/// Hard cap on that wait: past it the model should poll again rather than hold
/// the turn.
const MAX_WAIT: Duration = Duration::from_millis(60_000);
/// Polling period of the output buffer during a wait.
const POLL: Duration = Duration::from_millis(100);
/// Cap of what a session keeps between two reads (tail preserved).
const MAX_BUFFER: usize = crate::tool::MAX_TOOL_OUTPUT_BYTES;

/// State of the process behind a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Running,
    Exited(Option<i32>),
}

/// One live shell. Dropping it kills the process TREE: the session outlives a
/// tool call, so nothing else would (AC4).
struct Session {
    pid: Option<u32>,
    stdin: Option<tokio::process::ChildStdin>,
    buffer: Arc<Mutex<Vec<u8>>>,
    status: Arc<Mutex<SessionStatus>>,
    last_activity: Instant,
    command: String,
}

impl Session {
    fn status(&self) -> SessionStatus {
        self.status
            .lock()
            .map(|s| *s)
            .unwrap_or(SessionStatus::Running)
    }

    /// Drains what the process produced since the last read.
    fn drain(&self) -> String {
        let mut taken = Vec::new();
        if let Ok(mut buf) = self.buffer.lock() {
            std::mem::swap(&mut taken, &mut buf);
        }
        String::from_utf8_lossy(&taken).into_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Synchronous on purpose: `Drop` cannot await, and a session dropped at
        // the end of the program must not leave an orphan behind.
        if let Some(pid) = self.pid {
            kill_group_blocking(pid);
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

#[derive(Default)]
struct Store {
    sessions: HashMap<u64, Session>,
    next_id: u64,
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

    /// Closes every session and kills every process tree (AC4). Called when the
    /// Pyxis session ends; also what `Drop` falls back on.
    pub fn shutdown(&self) {
        let drained: Vec<Session> = match self.inner.lock() {
            Ok(mut store) => store.sessions.drain().map(|(_, s)| s).collect(),
            Err(_) => Vec::new(),
        };
        // Dropped OUTSIDE the lock: each `Drop` kills a process tree, and
        // holding the mutex across that would serialize a watchdog onto it.
        drop(drained);
    }

    fn insert(&self, session: Session) -> Result<u64, ToolError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| ToolError::Io("session store poisoned".to_string()))?;
        if store.sessions.len() >= MAX_SESSIONS {
            return Err(ToolError::Rejected(format!(
                "too many open shell sessions ({MAX_SESSIONS}): close one before opening another"
            )));
        }
        store.next_id += 1;
        let id = store.next_id;
        store.sessions.insert(id, session);
        Ok(id)
    }

    /// Removes a session and returns it, so the caller drops it outside the lock.
    fn take(&self, id: u64) -> Option<Session> {
        self.inner.lock().ok()?.sessions.remove(&id)
    }

    /// Runs `f` on a live session, refreshing its activity stamp.
    fn with_session<T>(&self, id: u64, f: impl FnOnce(&mut Session) -> T) -> Result<T, ToolError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| ToolError::Io("session store poisoned".to_string()))?;
        let idle = self.idle_timeout;
        let session = store.sessions.get_mut(&id).ok_or_else(|| {
            ToolError::Rejected(format!(
                "unknown shell session {id}: it was closed, or it expired after \
                 {}s of inactivity",
                idle.as_secs()
            ))
        })?;
        session.last_activity = Instant::now();
        Ok(f(session))
    }

    /// Watchdog of one session (AC3): closes it once idle past the timeout, or
    /// once the process has exited and its output has been read.
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
                            store.sessions.remove(&id)
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
    pub command: String,
    /// How long to wait for output before handing back, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct ExecCommand;

#[async_trait]
impl Tool for ExecCommand {
    type Input = ExecCommandInput;

    fn name(&self) -> &str {
        "exec_command"
    }
    fn description(&self) -> String {
        format!(
            "Run a command in a PERSISTENT shell session ({} -c) whose standard \
             input stays open. Use it for anything that asks a question, waits \
             for a keypress or runs long; `bash` stays the right tool for a \
             one-shot command. Returns the output produced within the wait plus, \
             when the process is still running, a session_id to write to with \
             write_stdin. A session is closed after {}s of inactivity. \
             Parameters: command, timeout_ms (optional).",
            crate::shell::resolve().label,
            IDLE_TIMEOUT.as_secs()
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to run in the session." },
                "timeout_ms": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Wait for output in milliseconds, or null for the default."
                }
            },
            "required": ["command", "timeout_ms"],
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
    /// AC5: the session inherits the perimeter in force. The command goes
    /// through the same pre-execution guardrail as `bash`, which is what stops a
    /// session from becoming the way around it.
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_shell_payload(&input.command, ctx)
    }
    /// Same baseline as `bash`: the decision follows the command.
    fn permission(&self, input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        match crate::command::classify(&input.command) {
            crate::command::CommandClass::SideEffectFree(_) => PermissionDecision::Allow,
            crate::command::CommandClass::Argv(_) | crate::command::CommandClass::Opaque(_) => {
                PermissionDecision::Ask
            }
        }
    }
    fn approval_memo(&self, input: &Self::Input) -> crate::permission::ApprovalMemo {
        use crate::permission::ApprovalMemo;
        match crate::command::classify(&input.command) {
            crate::command::CommandClass::SideEffectFree(tokens)
            | crate::command::CommandClass::Argv(tokens) => ApprovalMemo::Key(tokens),
            crate::command::CommandClass::Opaque(reason) => ApprovalMemo::Refused(reason),
        }
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let sessions = ctx.sessions.clone();
        let shell = crate::shell::resolve();
        let mut child = spawn_session(&shell, &input.command, ctx)?;
        let pid = child.id();
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(SessionStatus::Running));
        let stdin = child.stdin.take();
        pump(child.stdout.take(), Arc::clone(&buffer));
        pump(child.stderr.take(), Arc::clone(&buffer));
        {
            let status = Arc::clone(&status);
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                if let Ok(mut slot) = status.lock() {
                    *slot = SessionStatus::Exited(code);
                }
            });
        }
        let session = Session {
            pid,
            stdin,
            buffer: Arc::clone(&buffer),
            status: Arc::clone(&status),
            last_activity: Instant::now(),
            command: input.command.clone(),
        };
        let id = sessions.insert(session)?;
        sessions.watch(id);

        let collected = collect(&sessions, id, wait_of(input.timeout_ms), ctx).await?;
        Ok(finish(&sessions, id, collected))
    }
}

// ───────────────────────── write_stdin ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteStdinInput {
    pub session_id: u64,
    /// Text written to the session's standard input, verbatim. Add the newline
    /// yourself when the program waits for a line.
    pub input: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
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
         end it with a newline when the program waits for a line. Parameters: \
         session_id, input, timeout_ms (optional)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "integer", "minimum": 1, "description": "Session returned by exec_command." },
                "input": { "type": "string", "description": "Text written to standard input, verbatim." },
                "timeout_ms": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "description": "Wait for output in milliseconds, or null for the default."
                }
            },
            "required": ["session_id", "input", "timeout_ms"],
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
    /// the protected subpaths.
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_shell_payload(&input.input, ctx)
    }
    /// Fail-closed: stdin is opaque by nature (it feeds a program whose state we
    /// do not know), so it is never auto-approved and never remembered.
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let sessions = ctx.sessions.clone();
        let id = input.session_id;
        // The write happens on the session's own stdin handle, taken out of the
        // store for the duration so the lock is never held across an await.
        let mut stdin = sessions.with_session(id, |session| match session.status() {
            SessionStatus::Exited(code) => Err(ToolError::Rejected(format!(
                "shell session {id} has exited ({}): open a new one",
                code.map(|c| format!("exit code {c}"))
                    .unwrap_or_else(|| "terminated by signal".to_string())
            ))),
            SessionStatus::Running => session.stdin.take().ok_or_else(|| {
                ToolError::Rejected(format!("shell session {id} has no open standard input"))
            }),
        })??;
        let write = async {
            stdin.write_all(input.input.as_bytes()).await?;
            stdin.flush().await
        };
        let outcome = write.await;
        // Handed back whatever happened: a failed write must not turn the
        // session into one that can never be written to again.
        let _ = sessions.with_session(id, |session| session.stdin = Some(stdin));
        outcome.map_err(|e| ToolError::Io(format!("shell session {id} stdin: {e}")))?;

        let collected = collect(&sessions, id, wait_of(input.timeout_ms), ctx).await?;
        Ok(finish(&sessions, id, collected))
    }
}

const EXEC_GUIDELINES: &[&str] = &[
    "exec_command / write_stdin: use them ONLY for a command that waits for \
     input or runs long. Everything else goes through bash, which is one-shot \
     and leaves no session behind. Always answer a prompt with write_stdin on \
     the session_id returned, never by re-running the command.",
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

fn wait_of(timeout_ms: Option<u64>) -> Duration {
    match timeout_ms {
        Some(ms) => Duration::from_millis(ms).min(MAX_WAIT),
        None => DEFAULT_WAIT,
    }
}

/// Spawns the session process: same hardening as `bash`, plus an OPEN standard
/// input, which is the whole point of the story.
fn spawn_session(
    shell: &crate::shell::ShellChoice,
    command: &str,
    ctx: &ToolCtx,
) -> Result<tokio::process::Child, ToolError> {
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
        cmd.process_group(0);
        cmd
    };
    cmd.current_dir(&ctx.workspace)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    if let Some(harden) = &ctx.harden {
        harden(&mut cmd);
    }
    cmd.spawn()
        .map_err(|e| ToolError::Io(format!("shell session launch: {e}")))
}

/// Reads a stream into the session buffer until EOF. The buffer keeps the TAIL,
/// same policy as `bash`: on a chatty process the end is what matters.
fn pump<R>(reader: Option<R>, buffer: Arc<Mutex<Vec<u8>>>)
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
                        out.extend_from_slice(&buf[..n]);
                        if out.len() > MAX_BUFFER {
                            let overflow = out.len() - MAX_BUFFER;
                            out.drain(0..overflow);
                        }
                    }
                }
            }
        }
    });
}

/// What a wait produced.
struct Collected {
    text: String,
    status: SessionStatus,
}

/// Waits for output, publishing it as it comes (AC1) through the fragment
/// channel every other tool already uses. Returns early once the process exits:
/// there is nothing more to wait for.
async fn collect(
    sessions: &ExecSessions,
    id: u64,
    wait: Duration,
    ctx: &ToolCtx,
) -> Result<Collected, ToolError> {
    let deadline = Instant::now() + wait;
    let mut text = String::new();
    loop {
        let (chunk, status) =
            sessions.with_session(id, |session| (session.drain(), session.status()))?;
        if !chunk.is_empty() {
            ctx.emit_output(chunk.clone());
            text.push_str(&chunk);
        }
        if let SessionStatus::Exited(_) = status {
            // One last drain: the reader tasks may have flushed between the two.
            let (tail, status) =
                sessions.with_session(id, |session| (session.drain(), session.status()))?;
            if !tail.is_empty() {
                ctx.emit_output(tail.clone());
                text.push_str(&tail);
            }
            return Ok(Collected { text, status });
        }
        if Instant::now() >= deadline {
            return Ok(Collected {
                text,
                status: SessionStatus::Running,
            });
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

/// Shapes the result and closes the session when its process is gone: an exited
/// session must not stay in the store waiting for the watchdog.
fn finish(sessions: &ExecSessions, id: u64, collected: Collected) -> ToolOutput {
    let mut body = collected.text;
    if body.len() > MAX_BUFFER {
        body = truncate_tail(&body, MAX_BUFFER);
    }
    match collected.status {
        SessionStatus::Exited(code) => {
            let closed = sessions.take(id);
            // The command is kept on the session for the trace below, which
            // stays at `trace` level: a command line can carry a token, and the
            // observability policy (US-020) keeps call content out of `debug`.
            let command = closed
                .as_ref()
                .map(|s| s.command.clone())
                .unwrap_or_default();
            drop(closed);
            if body.is_empty() {
                body.push_str("(no output)");
            }
            match code {
                Some(0) => {
                    body.push_str("\n[session closed, exit code 0]");
                    ToolOutput::text(body)
                }
                Some(n) => {
                    body.push_str(&format!("\n[session closed, exit code {n}]"));
                    tracing::debug!(
                        target: "pyxis::tools",
                        session = id,
                        exit_code = n,
                        "shell session exited with a failure"
                    );
                    tracing::trace!(
                        target: "pyxis::tools",
                        session = id,
                        %command,
                        "shell session command"
                    );
                    ToolOutput::error(body)
                }
                None => {
                    body.push_str("\n[session closed, terminated by signal]");
                    ToolOutput::error(body)
                }
            }
        }
        SessionStatus::Running => {
            if body.is_empty() {
                body.push_str("(no output yet)");
            }
            body.push_str(&format!(
                "\n[session {id} still running; write to it with write_stdin \
                 (session_id={id}), or call it again to read more]"
            ));
            ToolOutput::text(body)
        }
    }
}

/// Kills a process group without awaiting. Used by `Drop`, which cannot.
fn kill_group_blocking(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let group = format!("-{pid}");
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(&group)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn a_command_that_ends_closes_its_session() {
        let ctx = ctx();
        let out = ExecCommand
            .call(
                ExecCommandInput {
                    command: "echo hello".to_string(),
                    timeout_ms: Some(5_000),
                },
                &ctx,
            )
            .await
            .expect("a simple command must run");
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("exit code 0"), "{}", out.content);
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
            .call(
                ExecCommandInput {
                    command: "read line; echo \"got:$line\"".to_string(),
                    timeout_ms: Some(500),
                },
                &ctx,
            )
            .await
            .expect("the session must open");
        assert!(
            opened.content.contains("still running"),
            "the process must be waiting for input: {}",
            opened.content
        );
        assert_eq!(ctx.sessions.len(), 1);

        let id: u64 = 1;
        let answered = WriteStdin
            .call(
                WriteStdinInput {
                    session_id: id,
                    input: "pyxis\n".to_string(),
                    timeout_ms: Some(5_000),
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

    // AC1: the output is handed back AS IT COMES, on the fragment channel every
    // other tool already uses, not only in the final result.
    #[tokio::test]
    async fn session_output_travels_on_the_fragment_channel() {
        let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let chunks = Arc::clone(&chunks);
            Arc::new(move |chunk: String| {
                if let Ok(mut c) = chunks.lock() {
                    c.push(chunk);
                }
            })
        };
        let ctx = ctx().with_output_sink(sink);
        let out = ExecCommand
            .call(
                ExecCommandInput {
                    command: "echo un; sleep 0.2; echo deux".to_string(),
                    timeout_ms: Some(5_000),
                },
                &ctx,
            )
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
    async fn an_unknown_session_names_the_cause() {
        let ctx = ctx();
        let err = WriteStdin
            .call(
                WriteStdinInput {
                    session_id: 999,
                    input: "x\n".to_string(),
                    timeout_ms: Some(100),
                },
                &ctx,
            )
            .await
            .expect_err("an unknown session must be refused");
        assert!(err.to_string().contains("unknown shell session"), "{err}");
    }

    #[tokio::test]
    async fn the_session_cap_is_enforced() {
        let ctx = ctx();
        for _ in 0..MAX_SESSIONS {
            ExecCommand
                .call(
                    ExecCommandInput {
                        command: "sleep 30".to_string(),
                        timeout_ms: Some(50),
                    },
                    &ctx,
                )
                .await
                .expect("opening a session below the cap must succeed");
        }
        let err = ExecCommand
            .call(
                ExecCommandInput {
                    command: "sleep 30".to_string(),
                    timeout_ms: Some(50),
                },
                &ctx,
            )
            .await
            .expect_err("past the cap the session must be refused");
        assert!(err.to_string().contains("too many open"), "{err}");
        ctx.sessions.shutdown();
        assert!(ctx.sessions.is_empty());
    }

    #[tokio::test]
    async fn shutdown_terminates_the_children() {
        let ctx = ctx();
        ExecCommand
            .call(
                ExecCommandInput {
                    command: "sleep 30".to_string(),
                    timeout_ms: Some(50),
                },
                &ctx,
            )
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
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "the process tree must be dead after shutdown");
    }

    #[test]
    fn stdin_goes_through_the_protected_subpath_guardrail() {
        let ctx = ctx();
        // A session must not become the way around `guard_command_paths`.
        let err = WriteStdin
            .validate_input(
                &WriteStdinInput {
                    session_id: 1,
                    input: "echo pwned > .git/hooks/pre-commit\n".to_string(),
                    timeout_ms: None,
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
            .call(
                ExecCommandInput {
                    command: "sleep 30".to_string(),
                    timeout_ms: Some(50),
                },
                &ctx,
            )
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
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "no orphan process may survive the closing");
    }

    #[test]
    fn the_wait_is_capped() {
        assert_eq!(wait_of(None), DEFAULT_WAIT);
        assert_eq!(wait_of(Some(10)), Duration::from_millis(10));
        assert_eq!(wait_of(Some(u64::MAX)), MAX_WAIT);
    }
}
