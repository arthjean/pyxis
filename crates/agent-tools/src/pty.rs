//! PTY backing for `exec_command` (EP-004 / US-014). Codex allocates a
//! pseudo-terminal when `tty: true`
//! (`codex-rs/core/src/unified_exec/process.rs`); a pipe cannot replace it,
//! because a program that checks `isatty` takes its non-interactive branch and
//! then behaves differently from the same command run by Codex.
//!
//! Three syscalls, no more: `openpty` creates the pair, `setsid` + `TIOCSCTTY`
//! make the slave the CONTROLLING terminal of the child (without which a shell
//! refuses job control and `^C` reaches nobody), and the master is driven
//! non-blocking through `AsyncFd` so a chatty process never parks a Tokio
//! worker.
//!
//! The master is the ONLY handle the parent keeps: stdout and stderr of a PTY
//! are the same stream by construction, so a session reads one buffer and
//! writes to that same fd.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::unix::AsyncFd;

/// Terminal size announced to the child. Codex reports the client's real size;
/// Pyxis has no client geometry at this layer, so it announces the POSIX
/// default rather than lying about a width the model would format against.
const ROWS: u16 = 24;
const COLS: u16 = 80;

/// A spawned PTY session: the child plus the master side of its terminal.
pub struct PtySession {
    pub child: tokio::process::Child,
    pub master: Arc<PtyMaster>,
}

/// Master side of a pseudo-terminal, readable and writable from Tokio.
pub struct PtyMaster(AsyncFd<File>);

impl PtyMaster {
    /// Writes the whole payload to the terminal. Partial writes are normal on a
    /// PTY (the line discipline buffer is small), hence the loop.
    pub async fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut rest = data;
        while !rest.is_empty() {
            let mut guard = self.0.writable().await?;
            match guard.try_io(|inner| inner.get_ref().write(rest)) {
                Ok(Ok(0)) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(Ok(written)) => rest = &rest[written..],
                Ok(Err(e)) => return Err(e),
                // Not actually writable: the guard is cleared, wait again.
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Reads what the child produced. Returns `Ok(0)` at end of stream: on
    /// Linux the master reports `EIO` once the last slave is closed, which is
    /// the normal end of a PTY, not a failure.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.0.readable().await?;
            match guard.try_io(|inner| inner.get_ref().read(buf)) {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(nix::libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Spawns `cmd` attached to a fresh pseudo-terminal. The caller has already set
/// the program, the cwd, the environment and the hardening: this function only
/// owns the terminal.
pub fn spawn(cmd: &mut tokio::process::Command) -> io::Result<PtySession> {
    let winsize = nix::pty::Winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pair = nix::pty::openpty(Some(&winsize), None).map_err(io::Error::from)?;
    let master: OwnedFd = pair.master;
    let slave: OwnedFd = pair.slave;
    nix::fcntl::fcntl(
        &master,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .map_err(io::Error::from)?;

    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: `adopt_terminal` runs between `fork` and `exec` and calls nothing
    // that is not async-signal-safe.
    unsafe { cmd.pre_exec(adopt_terminal) };
    let child = cmd.spawn()?;
    // EVERY copy of the slave must go on the parent side, or the master never
    // sees the end of the stream and a finished command looks like a running
    // one forever. `Command` keeps the `Stdio` values it was handed, so
    // replacing them is what actually closes those three descriptors.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    drop(slave);

    Ok(PtySession {
        child,
        master: Arc::new(PtyMaster(AsyncFd::new(File::from(master))?)),
    })
}

/// Runs in the child, after `fork` and before `exec`. `setsid` gives it its own
/// session (so its pid IS its process group id, which is what makes a group
/// signal precise), and `TIOCSCTTY` on fd 0 attaches the terminal it inherited:
/// without it a shell has no controlling terminal and refuses job control.
fn adopt_terminal() -> io::Result<()> {
    nix::unistd::setsid().map_err(io::Error::from)?;
    // SAFETY: an ioctl on an fd the child owns, with no pointer argument.
    if unsafe { nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn echo() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("test -t 0 && echo tty || echo pipe");
        cmd.kill_on_drop(true);
        cmd
    }

    /// The whole point of the story: inside a PTY, a program that asks whether
    /// it talks to a terminal must be told YES.
    #[tokio::test]
    async fn a_command_spawned_on_a_pty_sees_a_terminal() {
        let mut cmd = echo();
        let mut session = super::spawn(&mut cmd).expect("the pty pair must open");
        let mut seen = Vec::new();
        let mut buf = [0_u8; 256];
        loop {
            match session.master.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        let _ = session.child.wait().await;
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("tty"), "{text:?}");
    }

    /// A PTY carries stdin: what is written on the master reaches the program.
    #[tokio::test]
    async fn what_is_written_on_the_master_reaches_the_child() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("read line; echo \"got:$line\"");
        cmd.kill_on_drop(true);
        let mut session = super::spawn(&mut cmd).expect("the pty pair must open");
        session
            .master
            .write_all(b"pyxis\n")
            .await
            .expect("the write must reach the terminal");
        let mut seen = Vec::new();
        let mut buf = [0_u8; 256];
        loop {
            match session.master.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        let _ = session.child.wait().await;
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("got:pyxis"), "{text:?}");
    }
}
