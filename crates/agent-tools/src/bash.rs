//! `bash` tool: runs a shell command in the workspace. SENSITIVE action
//! (possibly destructive/network) -> target of the taint defense (4.6) and `Ask` by
//! default. Untrusted output (stdout/stderr = external content). The Registry
//! wraps the call in a `timeout`; `kill_on_drop` kills the process when the
//! timeout expires (US-012 AC2 / US-003 unhappy path). US-012.

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use agent_core::event::OutputStream;
use agent_core::tools::ToolExecution;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::spill::{SpillError, SpillStore, SpillWriter};
use crate::tool::{MAX_COMMAND_BYTES, Tool, ToolCtx, ToolOutput, truncate_tail};

/// The tool name, in one place: it also names the spill files, and a rename
/// that moved only one of the two would leave artifacts nobody can attribute.
const NAME: &str = "bash";
/// Capture bound (avoids flooding the prompt with a giant output). Shared with
/// the other tool outputs.
const MAX_OUTPUT: usize = crate::tool::MAX_TOOL_OUTPUT_BYTES;
/// Worst-case cost of the marker [`truncate_tail`] prepends to what it keeps.
/// The sentence is fixed and the count it carries cannot exceed the twenty
/// digits of a `usize`, so reserving this much alongside the spill notice makes
/// the bounded body fit the cap instead of overshooting it by the marker.
const TAIL_MARKER_BYTES: usize = "[... output truncated,  bytes, beginning omitted]\n".len() + 20;
/// Output streaming (US-015): size and coalescing delay of the fragments,
/// and cap of a published fragment.
const STREAM_FLUSH_BYTES: usize = 4_096;
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const STREAM_CHUNK_MAX: usize = 8_192;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    pub command: String,
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    type Input = BashInput;

    fn name(&self) -> &str {
        NAME
    }
    /// Runs a command: the clients get the command itself, not a JSON blob to
    /// re-parse.
    fn call_kind(&self, input: &serde_json::Value) -> agent_core::event::ToolCallKind {
        match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) => agent_core::event::ToolCallKind::Exec {
                command: command.to_string(),
                cwd: None,
            },
            None => agent_core::event::ToolCallKind::Other,
        }
    }
    fn description(&self) -> String {
        #[cfg(windows)]
        {
            "Run a PowerShell command (powershell.exe -NoProfile -NonInteractive -Command) in the workspace and return \
             stdout/stderr plus the exit code. The command runs under a timeout. \
             Parameter: command."
                .to_string()
        }
        // US-014: the description names the shell ACTUALLY used, the same as the
        // one announced in the `<environment>` block.
        #[cfg(not(windows))]
        {
            format!(
                "Run a shell command ({} -c) in the workspace and return \
                 stdout/stderr plus the exit code. The command runs under a timeout. \
                 Parameter: command.",
                crate::shell::resolve().label
            )
        }
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    // Fail-closed defaults kept: not read-only, not concurrent, SENSITIVE,
    // untrusted. We spell them out for readability.
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
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.command.trim().is_empty() {
            return Err(ValidationError::new("empty command"));
        }
        let bytes = input.command.len();
        if bytes > MAX_COMMAND_BYTES {
            return Err(ValidationError::new(format!(
                "command too large: {bytes} bytes > {MAX_COMMAND_BYTES}"
            )));
        }
        // US-002: the read-only subpaths of a writable root are subtracted HERE,
        // before execution, because the kernel cannot subtract what it granted.
        // Pre-permission, so no mode lifts it.
        crate::path::guard_command_paths(&ctx.sandbox, &ctx.workspace, &input.command)
    }
    /// US-007: the decision follows the command. A program of the
    /// side-effect-free set invoked with harmless arguments runs without a
    /// question; everything else keeps the historical `Ask`. The taint defense is
    /// orthogonal and still applies on top of this baseline (`resolve_permission`).
    fn permission(&self, input: &Self::Input, ctx: &PermCtx) -> PermissionDecision {
        match crate::command::classify_with(&input.command, &ctx.command_policy) {
            crate::command::CommandClass::SideEffectFree(_) => PermissionDecision::Allow,
            crate::command::CommandClass::Argv(_) | crate::command::CommandClass::Opaque(_) => {
                PermissionDecision::Ask
            }
        }
    }
    /// US-008: an answer is remembered under the EXACT argv token sequence. A
    /// command carrying a shell construct is never rememberable, and says why.
    fn approval_memo(&self, input: &Self::Input) -> crate::permission::ApprovalMemo {
        use crate::permission::ApprovalMemo;
        match crate::command::classify(&input.command) {
            crate::command::CommandClass::SideEffectFree(tokens)
            | crate::command::CommandClass::Argv(tokens) => ApprovalMemo::Key(tokens),
            crate::command::CommandClass::Opaque(reason) => ApprovalMemo::Refused(reason),
        }
    }
    fn timeout(&self, ctx: &ToolCtx) -> std::time::Duration {
        ctx.timeout
            .checked_add(ctx.cleanup_grace)
            .unwrap_or(ctx.timeout)
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let shell = crate::shell::resolve();
        // US-004: position in the confinement's block log, taken BEFORE the
        // command runs. What appears after it is this call's doing.
        let sandbox_mark = ctx.sandbox_observer.as_ref().map(|o| o.mark());
        let mut child = match build_command(&shell, &input.command, ctx).spawn() {
            Ok(child) => child,
            // AC4: a login shell that cannot be found or refuses to start does not
            // fail the turn. We fall back on `sh` for this command, and the
            // process-wide flag aligns what is announced to the model from the next
            // turn on.
            Err(first) if !shell.is_fallback() => {
                crate::shell::mark_login_shell_unusable();
                let fallback = crate::shell::resolve();
                build_command(&fallback, &input.command, ctx)
                    .spawn()
                    .map_err(|e| {
                        ToolError::Io(format!(
                            "shell launch: {} unusable ({first}), fallback {} failed: {e}",
                            shell.label, fallback.label
                        ))
                    })?
            }
            Err(e) => return Err(ToolError::Io(format!("shell launch: {e}"))),
        };
        let pid = child.id();
        // US-008 AC4: an interrupted turn drops this future instead of polling
        // it to the end. `kill_on_drop` then reaps the shell alone and leaves its
        // children (a `cargo`, a `make`, a dev server) running past the terminal
        // event. The guard signals the whole GROUP on the way out.
        let mut reaper = GroupReaper { pid };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // US-015: both streams are streamed to the client as they come, on top
        // of being captured for the final result.
        let stdout_sink = ctx.output.clone();
        let stderr_sink = ctx.output.clone();
        // US-076: both readers can write what they are about to drop. One file
        // per stream, because the two tasks run concurrently and an
        // interleaving of them is an order neither could state.
        let stdout_spill = stream_spill(ctx, "stdout");
        let stderr_spill = stream_spill(ctx, "stderr");
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(out) => read_tail(out, stdout_sink, OutputStream::Stdout, stdout_spill).await,
                None => Capture::default(),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(err) => read_tail(err, stderr_sink, OutputStream::Stderr, stderr_spill).await,
                None => Capture::default(),
            }
        });

        let mut cleanup_timed_out = false;
        let (status, timed_out) = match tokio::time::timeout(ctx.timeout, child.wait()).await {
            Ok(res) => (
                Some(res.map_err(|e| ToolError::Io(format!("shell wait: {e}")))?),
                false,
            ),
            Err(_) => {
                let cleanup = async {
                    if let Some(pid) = pid {
                        kill_process_tree(pid).await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                };
                cleanup_timed_out = tokio::time::timeout(ctx.cleanup_grace, cleanup)
                    .await
                    .is_err();
                (None, true)
            }
        };
        // The command reached an end of its own (exit or timeout cleanup): the
        // group must not be signalled again, or a recycled pid would be.
        reaper.disarm();

        let (stdout, stderr) = if cleanup_timed_out {
            stdout_task.abort();
            stderr_task.abort();
            (Capture::default(), Capture::default())
        } else {
            let stdout = stdout_task
                .await
                .map_err(|e| ToolError::Io(format!("stdout read: {e}")))?;
            let stderr = stderr_task
                .await
                .map_err(|e| ToolError::Io(format!("stderr read: {e}")))?;
            (stdout, stderr)
        };

        let mut body = String::new();
        let stdout_text = String::from_utf8_lossy(&stdout.bytes);
        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        if stdout.omitted > 0 {
            body.push_str(&format!(
                "[... stdout truncated, {} bytes, beginning omitted]\n",
                stdout.omitted
            ));
        }
        if !stdout.is_empty() {
            body.push_str(&stdout_text);
        }
        if !stderr_text.is_empty() || stderr.omitted > 0 {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if stderr.omitted > 0 {
                body.push_str(&format!(
                    "[... stderr truncated, {} bytes, beginning omitted]\n",
                    stderr.omitted
                ));
            }
            body.push_str(&stderr_text);
        }
        // US-076: the notice is reserved INSIDE the cap and appended LAST.
        // This bounding keeps the TAIL, so a locator sitting at the head of the
        // body is precisely what it would cut, and the model would be told
        // bytes are missing without being told where they went.
        let notice = spill_notice(&stdout, &stderr);
        let notice_cost = notice.as_ref().map_or(0, |line| line.len() + 1);
        if body.len() + notice_cost > MAX_OUTPUT {
            body = truncate_tail(
                &body,
                MAX_OUTPUT.saturating_sub(notice_cost + TAIL_MARKER_BYTES),
            );
        }
        if let Some(notice) = notice {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&notice);
        }
        let produced = stdout.produced() + stderr.produced();
        // The record carries ONE handle and the notice names every file: stdout
        // comes first, a command flooding both streams being one whose stdout
        // is the flood.
        let spill_locator = stdout.locator.clone().or_else(|| stderr.locator.clone());

        let code = status.as_ref().and_then(std::process::ExitStatus::code);
        let execution = ToolExecution {
            exit_code: code,
            signal: status.as_ref().and_then(exit_signal),
            timed_out,
            cancelled: false,
            session_closed: false,
            stderr_tail: (!stderr_text.is_empty()).then(|| truncate_tail(&stderr_text, 8 * 1024)),
        };

        let output = if timed_out {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str("[tool timeout exceeded]");
            if cleanup_timed_out {
                body.push_str("\n[process-tree cleanup incomplete after timeout]");
            }
            ToolOutput::error(body)
        } else {
            match code {
                Some(0) => {
                    if body.is_empty() {
                        body.push_str("(no output, success)");
                    }
                    ToolOutput::text(body)
                }
                Some(n) => {
                    body.push_str(&format!("\n[exit code {n}]"));
                    finish_failure(body, ctx, sandbox_mark)
                }
                None => {
                    body.push_str("\n[terminated by signal]");
                    finish_failure(body, ctx, sandbox_mark)
                }
            }
        };
        // The record is what carries the locator past this crate: `bound_feedback`
        // preserves it and re-states it in its marker when the model profile
        // bounds the result a second time.
        let output = match spill_locator {
            Some(locator) => output.with_spill(produced, locator),
            None => output,
        };
        Ok(output.with_execution(execution))
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

/// Attributes a failed command to the confinement when the sandbox actually
/// refused something (US-004 AC1). The rule itself lives in `sandbox` and is
/// shared with every other tool that can be refused by the perimeter; what stays
/// here is only the call site.
fn finish_failure(body: String, ctx: &ToolCtx, mark: Option<usize>) -> ToolOutput {
    crate::sandbox::attributed_failure(ctx, mark, body)
}

/// Builds the shell command (same options as before US-014, only the executed
/// program becomes variable).
fn build_command(
    shell: &crate::shell::ShellChoice,
    command: &str,
    ctx: &ToolCtx,
) -> tokio::process::Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        // `-c` in non-interactive mode: no interactive initialization file
        // is read, the behavior stays that of a script shell.
        cmd.arg("-c").arg(command);
        cmd.process_group(0);
        cmd
    };

    cmd.current_dir(&ctx.workspace)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    // Sandbox hardening (network through HTTP_PROXY) injected by agent-cli.
    // The Landlock FS confinement is process-wide -> inherited by this subprocess.
    if let Some(harden) = &ctx.harden {
        harden(&mut cmd);
    }
    cmd
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    omitted: usize,
    /// Where the whole stream was written, when it overflowed and the spill
    /// worked. `None` covers all three of: it fit, no storage, a failed write.
    locator: Option<String>,
}

impl Capture {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    /// What the stream produced: what is still in memory plus what left it.
    /// The property `omitted + kept == produced` is what makes the omission
    /// count exact rather than an estimate.
    fn produced(&self) -> usize {
        self.bytes.len() + self.omitted
    }
}

/// Where a stream's overflow goes instead of being dropped (US-076).
///
/// `bash` owns its spill because nothing downstream can. The generic policy
/// (`crate::spill_policy`) only ever sees the FINAL result, and the head of a
/// chatty command left memory long before that result existed: a compilation
/// producing ten mebibytes leaves thirty kilobytes of tail and an exact count
/// of what was destroyed. The reference classes this case as deferred work;
/// Pyxis cannot defer it, `bash` being the one producer of that quantity.
struct StreamSpill {
    store: std::sync::Arc<SpillStore>,
    /// Descriptive only, and it names the STREAM as well as the call, since
    /// each stream gets its own file.
    call_id: String,
    /// Opened at the FIRST overflow and never before: a command that stays
    /// under the cap must leave nothing behind.
    writer: Option<SpillWriter>,
    /// A failed write disables the spill for the rest of the call.
    broken: bool,
}

impl StreamSpill {
    fn new(store: std::sync::Arc<SpillStore>, call_id: String) -> Self {
        Self {
            store,
            call_id,
            writer: None,
            broken: false,
        }
    }

    /// Writes bytes that are about to leave memory.
    ///
    /// Best effort, like the policy's: a failure costs the spill, never the
    /// command, which keeps running and ends exactly as it did before EP-024.
    /// What was already written stays on disk as an orphan the model is never
    /// told about, the same trade the policy makes when its notice does not fit.
    fn push(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        if self.writer.is_none() {
            match self.store.open(NAME, &self.call_id) {
                Ok(writer) => self.writer = Some(writer),
                Err(error) => return self.fail(error),
            }
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        if let Err(error) = writer.write(bytes) {
            self.fail(error);
        }
    }

    fn fail(&mut self, error: SpillError) {
        tracing::warn!(
            target: "pyxis::tools",
            tool = NAME,
            error = %error,
            "spill write failed; the command output stays truncated"
        );
        self.broken = true;
        // Dropping the writer closes the file AND drops the locator: a partial
        // spill must never be announced as the full output.
        self.writer = None;
    }

    /// Appends what never overflowed and hands back the locator, so the file is
    /// the WHOLE stream and not only the part the cap pushed out of it.
    fn finish(mut self, tail: &[u8]) -> Option<String> {
        let mut writer = self.writer.take()?;
        if let Err(error) = writer.write(tail) {
            self.fail(error);
            return None;
        }
        Some(writer.finish().locator)
    }
}

/// One spill per stream, or `None` when the run has no storage at all, which
/// means no spill rather than an implicit root (US-072).
fn stream_spill(ctx: &ToolCtx, stream: &str) -> Option<StreamSpill> {
    let store = ctx.spill.clone()?;
    // The identifier only makes the file inspectable; a call dispatched outside
    // the loop has none and gets a fixed word instead.
    let call_id = ctx.call_id.clone().unwrap_or_else(|| "no-call".to_string());
    Some(StreamSpill::new(store, format!("{call_id}-{stream}")))
}

/// The one line the model reads about the bytes that left its context.
///
/// Same shape as the generic policy's notice: omission counted, locator,
/// recovery, one parenthesized line. One line per stream, so a command that
/// flooded both is told about both.
fn spill_notice(stdout: &Capture, stderr: &Capture) -> Option<String> {
    let lines: Vec<String> = [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter_map(|(name, capture)| {
            let locator = capture.locator.as_ref()?;
            Some(format!(
                "(Omitted {} bytes from the start of {name}. Full {name} saved to {locator}. \
                 Read it with `read` using `offset` and `limit`, or search it with `grep`.)",
                capture.omitted
            ))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Reads a stream until EOF: captures the TAIL for the final result (truncation
/// policy unchanged) and, when a consumer is listening, publishes the output as
/// it comes (US-015).
async fn read_tail(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    sink: Option<crate::tool::OutputSink>,
    stream: OutputStream,
    mut spill: Option<StreamSpill>,
) -> Capture {
    let mut out = Capture::default();
    let mut buf = [0_u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        out.bytes.extend_from_slice(&buf[..n]);
        if sink.is_some() {
            pending.extend_from_slice(&buf[..n]);
            // Coalescing: at most one fragment per `STREAM_FLUSH_INTERVAL`, which
            // bounds the event traffic on a chatty output while keeping the display
            // latency well under a second.
            if pending.len() >= STREAM_FLUSH_BYTES || last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
            {
                flush_stream(&mut pending, sink.as_ref(), stream);
                last_flush = tokio::time::Instant::now();
            }
        }
        if out.bytes.len() > MAX_OUTPUT {
            let overflow = out.bytes.len() - MAX_OUTPUT;
            // US-076: the bytes leaving memory are written BEFORE they are
            // dropped, which is the whole difference between bounding an output
            // and destroying it.
            if let Some(spill) = spill.as_mut() {
                spill.push(&out.bytes[..overflow]);
            }
            out.bytes.drain(0..overflow);
            out.omitted = out.omitted.saturating_add(overflow);
        }
    }
    flush_stream(&mut pending, sink.as_ref(), stream);
    out.locator = spill.and_then(|spill| spill.finish(&out.bytes));
    out
}

/// Publishes the complete UTF-8 part of `pending` and keeps the remainder: a
/// multi-byte character cut by a read boundary must not be split across two
/// fragments, which would show as `U+FFFD` in a client decoding one at a time.
///
/// The bytes go out RAW. A payload that is not UTF-8 at all is forwarded as-is
/// rather than replaced: deciding what unreadable output looks like belongs to
/// the client, not to the pipeline.
fn flush_stream(
    pending: &mut Vec<u8>,
    sink: Option<&crate::tool::OutputSink>,
    stream: OutputStream,
) {
    let Some(sink) = sink else {
        pending.clear();
        return;
    };
    if pending.is_empty() {
        return;
    }
    let valid_up_to = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_up_to == 0 {
        // Longer than any single character: the remainder will never complete
        // into valid UTF-8, so it is emitted rather than left to grow.
        if pending.len() > 4 {
            sink(stream, std::mem::take(pending));
        }
        return;
    }
    let rest = pending.split_off(valid_up_to);
    let mut chunk = std::mem::replace(pending, rest);
    // Backstop: a giant fragment adds nothing to a bounded live display. The cut
    // walks back to a character boundary so the kept tail stays decodable.
    if chunk.len() > STREAM_CHUNK_MAX {
        let mut start = chunk.len() - STREAM_CHUNK_MAX;
        while start < chunk.len() && !is_char_boundary(&chunk, start) {
            start += 1;
        }
        chunk.drain(0..start);
    }
    sink(stream, chunk);
}

/// Is `index` the start of a UTF-8 character in `bytes`? Continuation bytes are
/// `10xxxxxx`, every other byte starts one.
fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    bytes.get(index).is_none_or(|byte| (*byte as i8) >= -0x40)
}

/// Kills the process GROUP of a command whose future is dropped before it ends
/// (US-008 AC4: a cancelled turn).
///
/// `Drop` cannot await, so the signal is sent with the blocking `std::process`,
/// exactly like the exec sessions do. Disarmed as soon as the command reached an
/// end of its own: signalling a group whose leader was reaped could reach a
/// process that recycled the pid.
struct GroupReaper {
    pid: Option<u32>,
}

impl GroupReaper {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for GroupReaper {
    fn drop(&mut self) {
        let Some(pid) = self.pid else { return };
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
        signal_group(pid, libc::SIGKILL);
    }
}

/// Kills a subprocess AND its group: a hook or a shell command usually spawns
/// children, and `kill_on_drop` only reaches the direct child. Shared with the
/// hook engine, which starts its processes with the same `process_group(0)`.
pub(crate) async fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(windows))]
    {
        signal_group(pid, libc::SIGTERM);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        signal_group(pid, libc::SIGKILL);
    }
}

/// Signals a whole process group, by syscall.
///
/// NOT by running `kill`. The argv this needs is `kill -KILL -<pgid>`, and the
/// leading `-` on the group is read differently by the two implementations in
/// circulation: util-linux takes it as a process group, procps takes it as
/// another signal option and then exits 0 having signalled NOTHING. Fedora
/// ships the first, Debian and Ubuntu the second, so the cleanup worked on the
/// workstation it was written on and was a silent no-op on the CI runner and
/// on any Ubuntu host: an interrupted turn left its `cargo`, its `make` or its
/// dev server running, and the only visible symptom was a test that could not
/// be reproduced locally.
///
/// `killpg` takes the group as a positive pgid and answers through `errno`,
/// with no PATH lookup, no argv to misparse and no process to spawn. The last
/// point matters on its own: this is called from a `Drop`, where spawning a
/// process blocks the runtime thread.
#[cfg(not(windows))]
pub(crate) fn signal_group(pid: u32, signal: i32) {
    // The group id IS the leader's pid: every command here is started with
    // `process_group(0)`.
    //
    // SAFETY: `killpg` reads two integers and touches no memory owned by this
    // process. A group that no longer exists is reported through `errno`,
    // which this cleanup path has nothing to do with: the processes are gone,
    // which is the outcome it wanted.
    unsafe {
        libc::killpg(pid as libc::pid_t, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_truncation_keeps_the_end_and_marks_omission() {
        let body: String = (0..10).map(|i| format!("line{i}\n")).collect();
        let out = truncate_tail(&body, 20);
        assert!(out.starts_with("[... output truncated, "));
        assert!(out.contains("bytes, beginning omitted]"));
        assert!(out.contains("line9"), "the end should be preserved: {out}");
        assert!(
            !out.contains("line0"),
            "the beginning should be omitted: {out}"
        );
    }

    #[test]
    fn tail_truncation_is_char_boundary_safe() {
        let body = "¢".repeat(100);
        let out = truncate_tail(&body, 51);
        assert!(out.contains("beginning omitted]"));
        assert!(out.ends_with('¢'));
    }

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(truncate_tail("short", 30_000), "short");
    }

    /// US-008 AC4: a cancelled turn drops the tool future instead of polling it
    /// to the end. What must not survive is the process TREE, not just the shell
    /// `kill_on_drop` reaps: a `cargo`, a `make` or a dev server started by the
    /// command would otherwise outlive the terminal event the user just saw.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn dropping_a_running_command_kills_its_whole_process_group() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("pyxis-bash-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pidfile = dir.join("grandchild.pid");
        let _ = std::fs::remove_file(&pidfile);

        let ctx = ToolCtx::new(dir.clone());
        let input = BashInput {
            command: format!("sleep 300 & echo $! > {}; sleep 300", pidfile.display()),
        };

        // Poll the call just long enough for the grandchild to exist, then let
        // the future be dropped at the end of the block: this is exactly what
        // the loop does when its dispatch is abandoned. The scope is what drops
        // it, since `tokio::pin!` rebinds the name to a pinned borrow.
        let grandchild = {
            let call = Bash.call(input, &ctx);
            tokio::pin!(call);
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut found = None;
            while Instant::now() < deadline {
                tokio::select! {
                    _ = &mut call => panic!("the command was supposed to still be running"),
                    () = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
                if let Ok(raw) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = raw.trim().parse::<u32>()
                {
                    found = Some(pid);
                    break;
                }
            }
            let found = found.expect("the grandchild announced its pid");
            assert!(alive(found), "the grandchild is running before the drop");
            found
        };

        // The kill is a signal, not a promise of immediacy: let the OS reap.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && alive(grandchild) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !alive(grandchild),
            "the grandchild {grandchild} outlived the dropped command"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Scoped workspace for the spill tests, same shape as the drop test's.
    fn spill_workspace(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pyxis-bash-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A context wired exactly as the binary wires it: a workspace, a spill
    /// root under it, and the identifier of the call in flight.
    fn spill_ctx(dir: &std::path::Path, thread: &str) -> (ToolCtx, std::sync::Arc<SpillStore>) {
        let store = std::sync::Arc::new(SpillStore::create(dir, thread).unwrap());
        let mut ctx = ToolCtx::new(dir.to_path_buf());
        ctx.spill = Some(std::sync::Arc::clone(&store));
        ctx.call_id = Some("c1".to_string());
        (ctx, store)
    }

    /// US-076 AC5: a spill that cannot be written costs the spill and nothing
    /// else. The command still ends, still succeeds, and still returns the
    /// bounded output it returned before EP-024, with no locator to a file that
    /// does not exist.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_spill_write_failure_leaves_the_command_succeeding() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = spill_workspace("spill-broken");
        let (ctx, store) = spill_ctx(&dir, "thread_broken");
        // The root exists and stays readable: only creating a file in it fails,
        // which is the failure that happens mid-drain rather than at startup.
        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(store.root(), perms).unwrap();

        let out = Bash
            .call(
                BashInput {
                    command: "dd if=/dev/zero bs=64k count=1 status=none | tr '\\0' 'x'"
                        .to_string(),
                },
                &ctx,
            )
            .await
            .expect("a spill failure is not a pipeline error");

        assert!(!out.is_error, "{}", out.content);
        assert!(out.truncation.is_none(), "no locator may be advertised");
        assert!(
            out.content.contains("beginning omitted"),
            "the omission must still be stated: {}",
            &out.content[..120.min(out.content.len())]
        );
        assert!(!out.content.contains(".pyxis/spill"));
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);

        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(store.root(), perms).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// US-076 unhappy path: a command cancelled while it is still producing
    /// leaves a partial file holding what was actually read, and leaves no file
    /// descriptor open on it.
    ///
    /// The reader task is not aborted by the cancellation: the killed process
    /// closes the pipe, the reader reaches EOF, writes the tail it still holds
    /// and drops its writer. That is what makes the partial file match what was
    /// read rather than stop at the last overflow.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_cancelled_command_leaves_a_partial_file_and_no_open_descriptor() {
        use std::time::{Duration, Instant};

        let dir = spill_workspace("spill-cancel");
        let (ctx, store) = spill_ctx(&dir, "thread_cancel");
        let input = BashInput {
            // Throttled on purpose: the file must exist while the command is
            // still producing, which is the state the cancellation must find.
            command: "for _ in $(seq 1 100); do dd if=/dev/zero bs=64k count=1 status=none \
                      | tr '\\0' 'x'; sleep 0.05; done"
                .to_string(),
        };

        // Poll the call just long enough for the spill file to appear, then let
        // the scope drop the future: this is what an interrupted turn does.
        {
            let call = Bash.call(input, &ctx);
            tokio::pin!(call);
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut opened = false;
            while Instant::now() < deadline && !opened {
                tokio::select! {
                    _ = &mut call => panic!("the command was supposed to still be running"),
                    () = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
                opened = std::fs::read_dir(store.root()).unwrap().count() > 0;
            }
            assert!(opened, "the spill file must exist before the cancellation");
        }

        // The kill is a signal, not a promise of immediacy: the reader drains
        // what is left, writes its tail and closes.
        let path = std::fs::read_dir(store.root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_open(&path) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !is_open(&path),
            "the cancellation left {} open",
            path.display()
        );

        let spilled = std::fs::read(&path).unwrap();
        assert!(
            !spilled.is_empty(),
            "the partial file must hold what was read"
        );
        assert!(
            spilled.iter().all(|byte| *byte == b'x'),
            "the partial file must hold what the command actually produced"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Does this process still hold `path` open? `/proc/self/fd` is the only
    /// answer that does not depend on the writer telling the truth about itself.
    #[cfg(not(windows))]
    fn is_open(path: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return false;
        };
        entries
            .filter_map(Result::ok)
            .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == path))
    }

    /// A pid is still running only if the kernel has a process behind it that
    /// is not a zombie.
    ///
    /// `kill -0` cannot tell the difference: it succeeds on a zombie, which is
    /// a process that already died and is only waiting to be reaped. How long
    /// that wait lasts is decided by whoever inherits the orphan, and nothing
    /// here controls that. Under `systemd --user` it is instantaneous, so a
    /// `kill -0` probe looks correct on a developer machine; under a CI
    /// runner or a container init that does not reap, the same dead process
    /// answers "alive" for as long as the test cares to ask, and the group
    /// kill under test gets blamed for a corpse it did kill.
    #[cfg(not(windows))]
    fn alive(pid: u32) -> bool {
        // `/proc/<pid>/stat` is `pid (comm) state ...`, and `comm` may itself
        // contain spaces and parentheses: the state is the first field after
        // the LAST `)`.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rfind(')')
            .and_then(|end| stat[end + 1..].split_whitespace().next())
            .is_some_and(|state| state != "Z")
    }
}
