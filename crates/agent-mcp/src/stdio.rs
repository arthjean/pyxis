//! Spawning a stdio MCP server: the subprocess, its bounded frame reader, and
//! the tail of its stderr.
//!
//! `rmcp`'s `TokioChildProcess` is deliberately not used. It hard-codes an
//! unbounded frame reader (see `frame`), and its only escape hatch, `split()`,
//! is deprecated to an `unimplemented!()`. Spawning here costs a dozen lines and
//! buys the bound plus explicit control of the three pipes.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::frame::{BoundedFrames, MAX_FRAME_BYTES};

/// Bytes of stderr kept for the diagnostics of a stdio server. A server that
/// fails its handshake states the reason there, and discarding it leaves the user
/// with a bare timeout.
const STDERR_TAIL_CAP: usize = 4_096;
/// Grace left to the stderr reader to drain after a failed handshake.
pub(crate) const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(200);
/// Grace left to a server to exit on its own once its stdin is closed, before
/// the `kill_on_drop` of its handle takes over.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// A spawned server, before the MCP handshake.
pub(crate) struct SpawnedServer {
    /// Kept so the process outlives the handshake. `kill_on_drop` is set, so
    /// dropping this kills the server: that is the lifecycle contract.
    pub(crate) child: Child,
    pub(crate) stdout: BoundedFrames<ChildStdout>,
    pub(crate) stdin: ChildStdin,
    pub(crate) stderr: Option<ChildStderr>,
}

/// Spawns `command` with the three pipes owned by us.
///
/// Server stderr is piped rather than inherited: `rmcp` would otherwise let a
/// chatty server write straight over the TUI, which owns the terminal.
pub(crate) fn spawn(command: &mut Command) -> std::io::Result<SpawnedServer> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let taken = |what: &str| std::io::Error::other(format!("{what} was already taken"));
    let stdout = child.stdout.take().ok_or_else(|| taken("stdout"))?;
    let stdin = child.stdin.take().ok_or_else(|| taken("stdin"))?;
    let stderr = child.stderr.take();
    Ok(SpawnedServer {
        child,
        stdout: BoundedFrames::new(stdout, MAX_FRAME_BYTES),
        stdin,
        stderr,
    })
}

/// Bounded tail of a stdio server's stderr, shared with its reader task.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<String>>);

impl StderrTail {
    /// Spawns the reader task feeding this tail, `None` when stderr was not piped.
    pub(crate) fn reading(
        &self,
        stderr: Option<ChildStderr>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let stderr = stderr?;
        let tail = self.clone();
        Some(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tail.push_line(&line);
            }
        }))
    }

    fn push_line(&self, line: &str) {
        let mut buf = match self.0.lock() {
            Ok(buf) => buf,
            Err(poisoned) => poisoned.into_inner(),
        };
        buf.push_str(line);
        buf.push('\n');
        // Drop from the front, on a char boundary: the tail is what diagnoses a
        // failure, the head is startup noise.
        while buf.len() > STDERR_TAIL_CAP {
            match buf.find('\n') {
                Some(cut) => {
                    buf.drain(..=cut);
                }
                None => {
                    let keep = buf
                        .char_indices()
                        .rev()
                        .map(|(idx, _)| idx)
                        .find(|idx| buf.len() - idx <= STDERR_TAIL_CAP)
                        .unwrap_or(0);
                    buf.drain(..keep);
                    break;
                }
            }
        }
    }

    pub(crate) fn snapshot(&self) -> String {
        match self.0.lock() {
            Ok(buf) => buf.trim().to_string(),
            Err(poisoned) => poisoned.into_inner().trim().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_the_last_lines_within_the_cap() {
        let tail = StderrTail::default();
        for i in 0..2_000 {
            tail.push_line(&format!("line {i} with some padding to grow the buffer"));
        }
        let snapshot = tail.snapshot();
        assert!(snapshot.len() <= STDERR_TAIL_CAP, "{}", snapshot.len());
        // The tail is what diagnoses: the last line survives, the first does not.
        assert!(snapshot.contains("line 1999"), "{snapshot}");
        assert!(!snapshot.contains("line 0 with"), "{snapshot}");
    }

    #[test]
    fn stderr_tail_survives_a_single_oversized_line() {
        let tail = StderrTail::default();
        tail.push_line(&"x".repeat(STDERR_TAIL_CAP * 3));
        assert!(tail.snapshot().len() <= STDERR_TAIL_CAP);
    }
}
