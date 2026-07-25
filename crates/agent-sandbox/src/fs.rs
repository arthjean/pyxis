//! Kernel-level FS confinement through Landlock (US-020 AC1). Policy: read
//! only over the whole hierarchy, read+write only under the workspace.
//!
//! **Must be called early, on the main thread, BEFORE the tokio runtime is
//! built**: a Landlock domain is inherited by the threads created
//! *after* the restriction and by the child processes. That way the tokio workers
//! AND the Bash subprocesses inherit the confinement, without the fragile post-fork
//! `pre_exec` (malloc deadlock risk). `restrict_self` is irreversible.
//!
//! Landlock does NOT filter the network (see ADR-7 R3) nor the D-Bus sockets
//! -> the keyring (Secret Service) and the provider (direct HTTPS) stay
//! functional; the network of the tools is filtered separately by the proxy.

/// Result of applying the FS sandbox, to be presented to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Effective kernel confinement (full FS policy supported by the kernel).
    Enforced,
    /// Landlock active but kernel too old to guarantee the whole policy.
    PartiallyEnforced,
    /// Kernel without effective Landlock support -> FS confinement **not** guaranteed.
    NotEnforced,
    /// Non-Linux platform -> FS sandbox disabled (Linux-first, AC3).
    UnsupportedPlatform,
}

impl SandboxStatus {
    /// Warning message when the confinement is not effective (`None` when OK).
    pub fn warning(&self) -> Option<&'static str> {
        match self {
            SandboxStatus::Enforced => None,
            SandboxStatus::PartiallyEnforced => Some(
                "filesystem sandbox partially applied (incomplete Landlock support on this kernel): reduced guarantees",
            ),
            SandboxStatus::NotEnforced => Some(
                "filesystem sandbox NOT applied (kernel lacks effective Landlock support): writes are not confined",
            ),
            SandboxStatus::UnsupportedPlatform => Some(
                "filesystem sandbox disabled (non-Linux): Pyxis is Linux-first; writes are not confined",
            ),
        }
    }
}

/// Writable root discarded at resolution time, with the reason to log (US-012 AC2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredRoot {
    pub path: std::path::PathBuf,
    pub reason: IgnoreReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Path not found or not resolvable (edge case #6).
    Missing,
    /// Existing path that is not a directory.
    NotADirectory,
    /// Root so broad that the confinement would lose all meaning (`/`, the home).
    TooBroad,
}

impl IgnoreReason {
    pub fn message(&self) -> &'static str {
        match self {
            IgnoreReason::Missing => "path not found",
            IgnoreReason::NotADirectory => "not a directory",
            IgnoreReason::TooBroad => {
                "root too broad (system root or entire home): confinement would be meaningless"
            }
        }
    }
}

/// Result of resolving the writable roots: what will be granted, and what
/// was discarded (to be logged by the caller).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WritableRoots {
    pub granted: Vec<std::path::PathBuf>,
    pub ignored: Vec<IgnoredRoot>,
}

impl WritableRoots {
    /// Borrowed view, the shape expected by [`enforce_process`].
    pub fn as_paths(&self) -> Vec<&std::path::Path> {
        self.granted
            .iter()
            .map(std::path::PathBuf::as_path)
            .collect()
    }
}

/// Resolves the writable roots granted in addition to the workspace (US-012).
///
/// The temporary directory (`$TMPDIR` then `/tmp`) is always a candidate: it is
/// the default, and without configuration the behavior is limited to it. Every
/// path is canonicalized (a symlinked `$TMPDIR` is common), deduplicated, and
/// discarded with a reason when it is absent, when it is not a directory, or when it
/// is broad enough to empty the confinement of its meaning.
///
/// `home` is passed explicitly (and not read from the environment) so that the
/// policy is testable without mutating the process environment.
pub fn resolve_writable_roots(
    configured: &[std::path::PathBuf],
    home: Option<&std::path::Path>,
) -> WritableRoots {
    let mut candidates: Vec<std::path::PathBuf> = vec![std::env::temp_dir()];
    candidates.push(std::path::PathBuf::from("/tmp"));
    candidates.extend(configured.iter().cloned());

    let home = home.and_then(|h| std::fs::canonicalize(h).ok());
    let mut out = WritableRoots::default();
    for candidate in candidates {
        let Ok(real) = std::fs::canonicalize(&candidate) else {
            push_ignored(&mut out, candidate, IgnoreReason::Missing);
            continue;
        };
        if !real.is_dir() {
            push_ignored(&mut out, candidate, IgnoreReason::NotADirectory);
            continue;
        }
        // Too broad: the system root, the home, or any ancestor of the
        // home (`/home`). Granting one of the three amounts to confining nothing.
        let too_broad =
            real.parent().is_none() || home.as_ref().is_some_and(|h| h.starts_with(&real));
        if too_broad {
            push_ignored(&mut out, candidate, IgnoreReason::TooBroad);
            continue;
        }
        if !out.granted.contains(&real) {
            out.granted.push(real);
        }
    }
    out
}

fn push_ignored(out: &mut WritableRoots, path: std::path::PathBuf, reason: IgnoreReason) {
    if out.ignored.iter().any(|i| i.path == path) {
        return;
    }
    out.ignored.push(IgnoredRoot { path, reason });
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("landlock: {0}")]
    Landlock(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Applies the process-wide FS confinement: RW under `workspace`, read-only
/// elsewhere. To be called on the main thread before the async runtime.
#[cfg(target_os = "linux")]
/// Devices whose use stays allowed under confinement: see the justification when
/// they are added in `enforce_process`. Writing to `/dev/null` has no effect, and
/// `/dev/tty` is already the user's terminal, inherited through stdout.
#[cfg(target_os = "linux")]
const STANDARD_DEVICES: &[&str] = &["/dev/tty", "/dev/null"];

/// `writable_files`: EXISTING files outside the workspace to which the process keeps
/// a write right (`~/.pyxis/settings.toml`). Scope deliberately reduced
/// to the file, never to its directory: the confinement stays true for the whole rest
/// of the home. A missing path is ignored (the Landlock rule requires an openable fd).
///
/// `writable_roots`: EXISTING directories granted full write access, like
/// the workspace (US-012). The temporary directory is one of them by default:
/// without it, any tooling going through `mktemp` fails and pushes the user
/// to disable the whole confinement. The list is resolved and filtered upstream
/// by [`resolve_writable_roots`]; here, a non-openable path is ignored.
pub fn enforce_process(
    workspace: &std::path::Path,
    writable_files: &[&std::path::Path],
    writable_roots: &[&std::path::Path],
) -> Result<SandboxStatus, SandboxError> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    let abi = ABI::V7;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        .create()
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        // Global read + execute: the provider, the D-Bus keyring and
        // path resolution stay functional. FS confidentiality is
        // not the goal of this policy, only write confinement.
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| SandboxError::Landlock(e.to_string()))?,
            AccessFs::from_read(abi),
        ))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        // Full access only under the workspace. ABI V7 covers the modern
        // write rights (`truncate`, `ioctl_dev`) when the kernel supports them.
        .add_rule(PathBeneath::new(
            PathFd::new(workspace).map_err(|e| SandboxError::Landlock(e.to_string()))?,
            AccessFs::from_all(abi),
        ))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?;

    // Additional writable roots (US-012): same policy as the workspace.
    // `from_all` grants the directory rights (creation, deletion, rename),
    // which `mktemp -d` and any tooling writing under `$TMPDIR` require.
    for root in writable_roots {
        let Ok(fd) = PathFd::new(root) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    // Standard devices: without them, the confinement breaks uses it never
    // targeted. `/dev/tty` carries the `TIOCGWINSZ` ioctl that crossterm queries for the
    // terminal size: refused, it falls back on `tput` and reads 80x24, which pins
    // the TUI in a corner of the screen. `/dev/null` is the write sink that part
    // of the tooling (git first) opens systematically. The `IoctlDev`
    // right can only be granted here: it is attached to the descriptor at
    // opening time, so a file opened after the enforcement never gets it.
    for device in STANDARD_DEVICES {
        let Ok(fd) = PathFd::new(device) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_file(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    // Per-file write: `from_file` is the subset applicable to a
    // regular file (`WriteFile`, `Truncate`, ...). `from_all` would add
    // directory rights, which the kernel refuses on a file: the ruleset
    // would then fall back to `PartiallyEnforced` and trigger a false
    // degraded-confinement warning. Since the creation rights live on the
    // parent directory, a file deleted after the enforcement is no longer
    // recreatable: saving then fails explicitly.
    for file in writable_files {
        let Ok(fd) = PathFd::new(file) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_file(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::Landlock(e.to_string()))?;

    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => SandboxStatus::Enforced,
        RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
        RulesetStatus::NotEnforced => SandboxStatus::NotEnforced,
    })
}

/// Outside Linux: explicit degradation (AC3). The FS sandbox is disabled;
/// the caller MUST warn the user through `SandboxStatus::warning`.
#[cfg(not(target_os = "linux"))]
pub fn enforce_process(
    _workspace: &std::path::Path,
    _writable_files: &[&std::path::Path],
    _writable_roots: &[&std::path::Path],
) -> Result<SandboxStatus, SandboxError> {
    Ok(SandboxStatus::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_present_when_not_fully_enforced() {
        assert!(SandboxStatus::Enforced.warning().is_none());
        assert!(SandboxStatus::PartiallyEnforced.warning().is_some());
        assert!(SandboxStatus::NotEnforced.warning().is_some());
        assert!(SandboxStatus::UnsupportedPlatform.warning().is_some());
    }

    // On Linux with a Landlock kernel, the real confinement is proven by the s5
    // spike (isolated process: restrict_self is irreversible). Here we only check that
    // the call does not panic and returns a coherent status, WITHOUT restricting the
    // test process (which must be able to keep writing).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_degrades() {
        let st = enforce_process(std::path::Path::new("/tmp"), &[], &[]).unwrap();
        assert_eq!(st, SandboxStatus::UnsupportedPlatform);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis-roots-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn temp_dir_is_granted_by_default() {
        let roots = resolve_writable_roots(&[], None);
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            roots.granted.contains(&tmp),
            "le répertoire temporaire doit être accordé sans configuration: {roots:?}"
        );
    }

    #[test]
    fn configured_root_is_granted_and_deduplicated() {
        let dir = scratch("granted");
        let roots = resolve_writable_roots(&[dir.clone(), dir.clone()], None);
        assert_eq!(
            roots.granted.iter().filter(|p| **p == dir).count(),
            1,
            "une racine répétée n'est accordée qu'une fois: {roots:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_root_is_ignored_with_a_reason() {
        let missing = std::env::temp_dir().join(format!("pyxis-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let roots = resolve_writable_roots(std::slice::from_ref(&missing), None);
        assert!(!roots.granted.contains(&missing));
        let ignored = roots
            .ignored
            .iter()
            .find(|i| i.path == missing)
            .expect("racine absente tracée");
        assert_eq!(ignored.reason, IgnoreReason::Missing);
    }

    #[test]
    fn file_root_is_ignored_as_not_a_directory() {
        let dir = scratch("file-root");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let roots = resolve_writable_roots(std::slice::from_ref(&file), None);
        assert!(!roots.granted.contains(&file));
        assert!(
            roots
                .ignored
                .iter()
                .any(|i| i.path == file && i.reason == IgnoreReason::NotADirectory)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_root_and_whole_home_are_refused() {
        let home = scratch("home");
        let roots = resolve_writable_roots(
            &[
                std::path::PathBuf::from("/"),
                home.clone(),
                // ancestor of the home: granting `/home` amounts to granting the home.
                home.parent().unwrap().to_path_buf(),
            ],
            Some(&home),
        );
        for refused in [
            std::path::PathBuf::from("/"),
            home.clone(),
            home.parent().unwrap().to_path_buf(),
        ] {
            assert!(
                !roots.granted.contains(&refused),
                "{} ne doit jamais être accordé: {roots:?}",
                refused.display()
            );
            assert!(
                roots
                    .ignored
                    .iter()
                    .any(|i| i.path == refused && i.reason == IgnoreReason::TooBroad),
                "{} doit être refusé comme trop large: {roots:?}",
                refused.display()
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
