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
use crate::tool::{MAX_COMMAND_BYTES, Tool, ToolCtx, ToolOutput, truncate_tail};

/// Capture bound (avoids flooding the prompt with a giant output). Shared with
/// the other tool outputs.
const MAX_OUTPUT: usize = crate::tool::MAX_TOOL_OUTPUT_BYTES;
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
        "bash"
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
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(out) => read_tail(out, stdout_sink, OutputStream::Stdout).await,
                None => Capture::default(),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(err) => read_tail(err, stderr_sink, OutputStream::Stderr).await,
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
        if body.len() > MAX_OUTPUT {
            body = truncate_tail(&body, MAX_OUTPUT);
        }

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
/// refused something (US-004 AC1). The cause is named IN the result the model
/// reads, which is what stops it from retrying variants of the same command,
/// and is carried structurally so the Registry can offer an escalation.
fn finish_failure(mut body: String, ctx: &ToolCtx, mark: Option<usize>) -> ToolOutput {
    let blocked = match (ctx.sandbox_observer.as_ref(), mark) {
        (Some(observer), Some(mark)) => observer.blocked_since(mark),
        _ => Vec::new(),
    };
    let allowed = ctx
        .sandbox_observer
        .as_ref()
        .map(|o| o.allowed())
        .unwrap_or_else(|| "none".to_string());
    let Some(denial) =
        crate::sandbox::classify_failure(ctx.sandbox_enforced, &blocked, &allowed, &body)
    else {
        return ToolOutput::error(body);
    };
    body.push('\n');
    body.push_str(&denial.explain());
    ToolOutput::error(body).with_denial(denial)
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
}

impl Capture {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Reads a stream until EOF: captures the TAIL for the final result (truncation
/// policy unchanged) and, when a consumer is listening, publishes the output as
/// it comes (US-015).
async fn read_tail(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    sink: Option<crate::tool::OutputSink>,
    stream: OutputStream,
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
            out.bytes.drain(0..overflow);
            out.omitted = out.omitted.saturating_add(overflow);
        }
    }
    flush_stream(&mut pending, sink.as_ref(), stream);
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
        {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pid}"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
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
        let group = format!("-{pid}");
        let _ = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg(&group)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tokio::process::Command::new("kill")
            .arg("-KILL")
            .arg(&group)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
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

    #[cfg(not(windows))]
    fn alive(pid: u32) -> bool {
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
