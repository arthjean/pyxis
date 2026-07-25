//! Shell used to run commands (US-014). SINGLE source for the `bash` tool
//! and for the `<environment>` block announced to the model: what Pyxis announces must
//! be what it runs, otherwise the model produces constructs that fail.
//!
//! The login shell is kept only when it is executable AND known to be
//! POSIX-compatible: `fish`, `nu` or `xonsh` accept `-c` but not the
//! syntax the model produces (`&&`, `2>&1`, `$(...)`, `export`), so
//! announcing them would be the same lie the other way around. Fallback: `sh`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Shells whose `-c` interprets the POSIX syntax the model generates.
const POSIX_SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "ksh", "ksh93", "mksh", "pdksh", "ash", "busybox",
];

/// Universal fallback: present on every POSIX system, reference semantics.
pub const FALLBACK: &str = "sh";

/// A login shell that refuses to start is observed at run time
/// (AC4). The flag is process-wide: the next turn therefore announces `sh` to the
/// model, instead of repeating a promise already broken.
static LOGIN_SHELL_UNUSABLE: AtomicBool = AtomicBool::new(false);

/// Selected shell: `program` is executed, `label` is announced to the model. Both
/// always designate the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellChoice {
    pub program: PathBuf,
    pub label: String,
}

impl ShellChoice {
    fn fallback() -> Self {
        Self {
            program: PathBuf::from(FALLBACK),
            label: FALLBACK.to_string(),
        }
    }

    /// True when this choice IS already the fallback (no second attempt possible).
    pub fn is_fallback(&self) -> bool {
        self.program == Path::new(FALLBACK)
    }
}

/// Shell actually used to run and to announce.
pub fn resolve() -> ShellChoice {
    #[cfg(windows)]
    {
        ShellChoice {
            program: PathBuf::from("powershell.exe"),
            label: "powershell.exe".to_string(),
        }
    }
    #[cfg(not(windows))]
    {
        if LOGIN_SHELL_UNUSABLE.load(Ordering::Relaxed) {
            return ShellChoice::fallback();
        }
        resolve_from(std::env::var_os("SHELL").as_deref())
    }
}

/// Pure decision (testable without mutating the process environment).
#[cfg(not(windows))]
pub fn resolve_from(login_shell: Option<&std::ffi::OsStr>) -> ShellChoice {
    let Some(raw) = login_shell
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return ShellChoice::fallback();
    };
    let known_posix = raw
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| POSIX_SHELLS.contains(&name));
    if !known_posix || !is_executable(&raw) {
        return ShellChoice::fallback();
    }
    let label = raw.to_string_lossy().into_owned();
    ShellChoice {
        program: raw,
        label,
    }
}

/// Reports that the login shell refused to start: subsequent calls,
/// including what is announced to the model, fall back on `sh`.
pub fn mark_login_shell_unusable() {
    LOGIN_SHELL_UNUSABLE.store(true, Ordering::Relaxed);
}

#[cfg(not(windows))]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn absent_login_shell_falls_back_to_sh() {
        let choice = resolve_from(None);
        assert_eq!(choice.label, "sh");
        assert!(choice.is_fallback());
        assert_eq!(resolve_from(Some(OsStr::new(""))).label, "sh");
    }

    #[test]
    fn missing_binary_falls_back_to_sh() {
        assert_eq!(
            resolve_from(Some(OsStr::new("/nonexistent/bin/bash"))).label,
            "sh"
        );
    }

    #[test]
    fn non_posix_login_shell_falls_back_to_sh() {
        // fish accepts `-c` but not `export VAR=1` nor `&&` the same way:
        // announcing it to the model would produce commands that fail.
        assert_eq!(resolve_from(Some(OsStr::new("/usr/bin/fish"))).label, "sh");
        assert_eq!(resolve_from(Some(OsStr::new("/usr/bin/nu"))).label, "sh");
    }

    #[test]
    fn executable_posix_login_shell_is_kept_and_announced_verbatim() {
        // `/bin/sh` exists everywhere this test runs; the label must be the
        // executed path, not an alias.
        let choice = resolve_from(Some(OsStr::new("/bin/sh")));
        assert_eq!(choice.program, PathBuf::from("/bin/sh"));
        assert_eq!(choice.label, "/bin/sh");
    }

    #[test]
    fn announced_shell_is_the_executed_one() {
        let choice = resolve();
        assert_eq!(choice.label, choice.program.to_string_lossy());
    }
}
