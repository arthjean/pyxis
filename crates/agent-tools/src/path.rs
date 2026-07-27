//! Path confinement to the workspace (application-level defense). LEXICAL
//! normalization (without touching the FS, hence valid even for a file to be
//! created): we resolve `.`/`..` and check that the result stays under the root.
//! Kernel-level anti-symlink/anti-escape enforcement is delegated to Landlock
//! (US-020, ARCHITECTURE section 4 / sandbox invariant): this is the first line,
//! not the only one.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use agent_core::sandbox::{SandboxPolicy, WriteRefusal};

use crate::error::{ToolError, ValidationError};

/// Workspace subpaths whose content runs LATER, outside the sandbox and
/// outside the proxy, or that drive Pyxis itself (US-013). An agent hijacked by
/// indirect injection would drop code there that the user's next `git commit`
/// would run on their machine (CVE-2026-26268); `.git/config` is enough
/// to redirect `core.hooksPath` or define an alias, and `.pyxis/` carries
/// the agent configuration and sessions ("configuration-based sandbox escape"
/// pattern).
///
/// `.git` is protected as a whole, and not only `hooks/` and `config`: in a git
/// worktree, `.git` is a FILE `gitdir: ...` whose rewrite moves the
/// configuration and hooks to a directory chosen by the attacker. Both
/// subpaths stay listed first so that the refusal names the exact zone.
///
/// **Scope (US-002): every tool, `bash` included.** Landlock is additive, so
/// the kernel cannot subtract this right under a workspace it already granted.
/// The subtraction therefore lives at POLICY level and is evaluated before
/// execution, which is the level a shell command can be tested at too
/// ([`guard_command_paths`]).
pub const PROTECTED_SUBPATHS: &[&str] = &[".git/hooks", ".git/config", ".git", ".pyxis"];

/// The confinement policy a bare workspace carries: writes confined to it, with
/// the deferred-execution subpaths subtracted. What `ToolCtx` starts from and
/// what the binary widens with the configured extra roots.
pub fn default_policy(workspace: &Path) -> SandboxPolicy {
    SandboxPolicy::workspace_write(
        workspace,
        Vec::new(),
        PROTECTED_SUBPATHS.iter().map(PathBuf::from),
    )
}

/// Normalizes lexically (resolves `.` and `..` without disk access, does not follow
/// symlinks). A `..` that climbs above the root is an escape.
fn lexical_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                // Absolute path: start over (it will be re-checked against base).
                out = PathBuf::from(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    Some(out)
}

/// Resolves `path` (absolute or relative to the workspace) and checks confinement.
/// Returns the absolute normalized path, or `OutsideWorkspace`.
pub fn confine(workspace: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        lexical_normalize(requested)
    } else {
        lexical_join(workspace, requested)
            .ok_or_else(|| ToolError::OutsideWorkspace(path.into()))?
    };
    let root = lexical_normalize(workspace);
    if joined.starts_with(&root) {
        Ok(joined)
    } else {
        Err(ToolError::OutsideWorkspace(path.into()))
    }
}

/// Write guardrail called from `validate_input`, hence BEFORE the permission
/// decision: refusing a deferred-execution zone (US-013) depends on no
/// mode and can be lifted neither by `DontAsk` nor by `BypassPermissions`.
/// A path outside the workspace is not handled here: it is refused by `confine` at
/// call time, with its dedicated error.
pub fn guard_protected_path(workspace: &Path, path: &str) -> Result<(), ValidationError> {
    guard_write_target(&default_policy(workspace), workspace, path)
}

/// Policy-aware form of [`guard_protected_path`] (US-001 AC2, US-002 AC1). The
/// read-only variant refuses first and names itself; otherwise the declared
/// read-only subpaths decide.
pub fn guard_write_target(
    policy: &SandboxPolicy,
    workspace: &Path,
    path: &str,
) -> Result<(), ValidationError> {
    if matches!(policy, SandboxPolicy::ReadOnly { .. }) {
        return Err(ValidationError::new(format!(
            "write refused: {path}: {}",
            WriteRefusal::ReadOnlyPolicy
        )));
    }
    let Ok(target) = confine(workspace, path) else {
        return Ok(());
    };
    ensure_policy_allows_write(policy, workspace, &target, path)
        .map_err(|e| ValidationError::new(e.to_string()))
}

/// Refuses a path that reaches a protected zone, directly or through a symlink.
pub fn ensure_not_protected(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    ensure_policy_allows_write(&default_policy(workspace), workspace, target, display_path)
}

/// Refuses a resolved path the policy keeps read-only, directly or through a
/// symlink. Called at execution time, after the target is known.
pub fn ensure_policy_allows_write(
    policy: &SandboxPolicy,
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    // Defence in depth: `validate_input` already refused, but a tool that
    // resolves its target late must not depend on that having happened.
    if matches!(policy, SandboxPolicy::ReadOnly { .. }) {
        return Err(ToolError::Rejected(format!(
            "write refused: {display_path}: {}",
            WriteRefusal::ReadOnlyPolicy
        )));
    }
    if let Some(zone) = read_only_zone_resolved(policy, workspace, target) {
        return Err(protected_error(display_path, &zone));
    }
    Ok(())
}

/// The read-only zone `target` reaches, lexically or after resolution.
///
/// Two passes on purpose: the lexical one decides without touching the disk (so
/// a path still to be created is covered), the resolved one catches a target
/// that only reaches the zone through a symlink (US-002 AC4). The resolved path
/// is re-anchored on the policy's own root, so the zones stay the declared ones.
fn read_only_zone_resolved(
    policy: &SandboxPolicy,
    workspace: &Path,
    target: &Path,
) -> Option<PathBuf> {
    if let Some(zone) = policy.read_only_zone(target) {
        return Some(zone);
    }
    let (Ok(real_root), Some(real_target)) = (
        std::fs::canonicalize(workspace),
        resolve_existing_prefix(target),
    ) else {
        return None;
    };
    let rel = real_target.strip_prefix(&real_root).ok()?;
    policy.read_only_zone(&lexical_normalize(workspace).join(rel))
}

fn protected_error(display_path: &str, zone: &Path) -> ToolError {
    ToolError::Rejected(format!(
        "write refused: {display_path} is a protected path ({}); its contents run outside the sandbox",
        zone.display()
    ))
}

/// Refuses a shell command that names a path the policy keeps read-only
/// (US-002). Evaluated BEFORE execution, hence before any permission decision:
/// this is the only level able to subtract a subpath from a root the kernel has
/// already granted, since Landlock rules only add.
///
/// **Fail-closed on the TARGET, not on the command** (US-002 AC5). A shell
/// command is never proven to write: a redirection, a substitution or a
/// computed argument makes the intent undecidable. So a command that is not
/// proven side-effect free is refused as soon as it NAMES a protected path,
/// whether or not the write can be demonstrated. Two consequences, both
/// deliberate:
/// - a command of the side-effect-free set is exempt, so `cat .git/config`
///   stays a read and stays allowed;
/// - a read performed by a program outside that set (`tar --exclude=.git ...`)
///   is refused. That false positive is the price of the guarantee, and one
///   confirmation-free alternative always exists: name no protected path.
///
/// Residual limit, documented in `docs/CURRENT_STATUS.md`: a path BUILT at run
/// time (`p=.g; cat ${p}it/config`) is not visible to any static analysis and
/// escapes this guard. Closing it would take a shell interpreter, which the
/// non-goals of the PRD exclude.
pub fn guard_command_paths(
    policy: &SandboxPolicy,
    workspace: &Path,
    command: &str,
) -> Result<(), ValidationError> {
    // Nothing to subtract: full access, or read-only where no root is granted
    // at all and the kernel is the authority.
    if policy.writable_roots().is_empty() {
        return Ok(());
    }
    let class = crate::command::classify(command);
    if matches!(class, crate::command::CommandClass::SideEffectFree(_)) {
        return Ok(());
    }
    let fragments: Vec<&str> = match class.tokens() {
        Some(tokens) => tokens.iter().map(String::as_str).collect(),
        // Opaque command: there is no argv to read, so the raw text is split on
        // everything a shell would interpret and every piece is tested.
        None => split_shell_fragments(command).collect(),
    };
    for candidate in path_candidates(fragments) {
        // Two tests, because a shell command has no knowable working directory.
        // `cd src && ... > ../.git/config` resolves UNDER the workspace at run
        // time while climbing out of it lexically, so anchoring alone would miss
        // it. Naming a protected zone anywhere in the components is therefore a
        // refusal on its own, and the anchored test only adds what it can prove
        // further: symlink resolution.
        let zone = names_read_only_zone(policy, Path::new(&candidate)).or_else(|| {
            let target = confine(workspace, &candidate).ok()?;
            read_only_zone_resolved(policy, workspace, &target)
        });
        if let Some(zone) = zone {
            return Err(ValidationError::new(format!(
                "command refused: `{candidate}` reaches a protected path ({}); \
                 its contents run outside the sandbox, and a shell command is never \
                 proven not to write to it",
                zone.display()
            )));
        }
    }
    Ok(())
}

/// Does `candidate` name a read-only zone ANYWHERE in its components?
///
/// Unanchored on purpose: it is the answer to a command whose working directory
/// cannot be known. The comparison stays component-wise, so `.gitignore` is
/// never `.git`, and a zone still has to appear as a contiguous run of
/// components, so `git/hooks` is not `.git/hooks`.
fn names_read_only_zone(policy: &SandboxPolicy, candidate: &Path) -> Option<PathBuf> {
    let parts: Vec<_> = candidate
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .map(Component::as_os_str)
        .collect();
    for root in policy.writable_roots() {
        for zone in &root.read_only_subpaths {
            let needle: Vec<_> = zone
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .map(Component::as_os_str)
                .collect();
            if !needle.is_empty() && parts.windows(needle.len()).any(|w| w == needle.as_slice()) {
                return Some(zone.clone());
            }
        }
    }
    None
}

/// Splits a raw command on whitespace AND on every character a shell
/// interprets. Coarser than a parser on purpose: the goal is to miss no path
/// fragment, not to reconstruct the command.
fn split_shell_fragments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| c.is_whitespace() || SHELL_SEPARATORS.contains(c))
        .filter(|fragment| !fragment.is_empty())
}

const SHELL_SEPARATORS: &str = ";&|<>$`(){}[]*?'\"\\!#";

/// Keeps the fragments that can designate a path, splitting the ones that carry
/// a value (`--exclude=.git`, `a:b`). Deduplicated: a repeated argument costs
/// one resolution, not one per occurrence.
fn path_candidates(fragments: Vec<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for fragment in fragments {
        for piece in fragment.split(['=', ',', ':']) {
            // A path fragment either contains a separator or opens on a dot,
            // which is what every protected zone does (`.git`, `.pyxis`).
            if piece.is_empty() || !(piece.contains('/') || piece.starts_with('.')) {
                continue;
            }
            out.insert(piece.to_string());
        }
    }
    out
}

/// Resolves the EXISTING part of `target` (canonicalized, hence symlinks followed) and
/// appends the components still missing. `None` when nothing resolves.
fn resolve_existing_prefix(target: &Path) -> Option<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = target.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut out = real;
            out.extend(missing.iter().rev());
            return Some(out);
        }
        missing.push(probe.file_name()?.to_os_string());
        if !probe.pop() {
            return None;
        }
    }
}

/// Checks that the deepest existing ancestor of `target` really resolves
/// under the workspace. To be called before `create_dir_all` so as not to create a
/// directory through a symlink/junction outside the workspace.
pub fn ensure_existing_ancestor_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    let root =
        std::fs::canonicalize(workspace).map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
    let mut probe = target;
    loop {
        match std::fs::symlink_metadata(probe) {
            Ok(_) => {
                let real = std::fs::canonicalize(probe)
                    .map_err(|e| ToolError::Io(format!("{}: {e}", probe.display())))?;
                return if real.starts_with(&root) {
                    Ok(())
                } else {
                    Err(ToolError::OutsideWorkspace(display_path.into()))
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                probe = probe
                    .parent()
                    .ok_or_else(|| ToolError::OutsideWorkspace(display_path.into()))?;
            }
            Err(e) => return Err(ToolError::Io(format!("{}: {e}", probe.display()))),
        }
    }
}

/// Checks that `target` itself, if it exists, does not resolve outside the workspace.
/// For a new file, checks its parent after the directories are created.
pub fn ensure_real_path_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    ensure_existing_ancestor_confined(workspace, target, display_path)?;
    if std::fs::symlink_metadata(target).is_ok() {
        let root = std::fs::canonicalize(workspace)
            .map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
        let real = std::fs::canonicalize(target)
            .map_err(|e| ToolError::Io(format!("{}: {e}", target.display())))?;
        if !real.starts_with(&root) {
            return Err(ToolError::OutsideWorkspace(display_path.into()));
        }
    }
    Ok(())
}

/// Checks that every existing component of `target` is a real path
/// under the workspace, without a symlink nor a reparse point. Native tools refuse
/// links on purpose, to prevent a checkout from controlling access to
/// files outside the workspace.
pub fn ensure_existing_path_no_links(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    walk_existing_components(workspace, target, display_path, false)?;
    ensure_real_path_confined(workspace, target, display_path)
}

/// Checks a path to create or replace. Existing parents must not
/// contain links; the target is checked too when it already exists.
pub fn ensure_creatable_path_no_links(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    walk_existing_components(workspace, target, display_path, true)?;
    ensure_real_path_confined(workspace, target, display_path)
}

/// Replaces a file with bounded content without writing through a final
/// symlink. The temporary file is created in the same parent with `create_new`,
/// then renamed onto the target after one last check.
pub async fn replace_file_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
    bytes: &[u8],
) -> Result<(), ToolError> {
    if let Some(parent) = target.parent() {
        ensure_existing_ancestor_confined(workspace, parent, display_path)?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Io(format!("creating parent directory: {e}")))?;
        ensure_existing_path_no_links(workspace, parent, display_path)?;
    }
    ensure_creatable_path_no_links(workspace, target, display_path)?;

    let parent = target
        .parent()
        .ok_or_else(|| ToolError::OutsideWorkspace(display_path.into()))?;
    let stem = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{stem}.pyxis-tmp-{}-{nonce}", std::process::id()));

    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
        file.write_all(bytes)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
        file.flush()
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
    }

    ensure_creatable_path_no_links(workspace, target, display_path)?;
    match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            if is_link_like(&meta) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ToolError::OutsideWorkspace(display_path.into()));
            }
            if meta.is_dir() {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ToolError::Rejected(format!(
                    "{display_path} is a directory, not a file"
                )));
            }
            tokio::fs::remove_file(target)
                .await
                .map_err(|e| ToolError::Io(format!("{}: {e}", target.display())))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(ToolError::Io(format!("{}: {e}", target.display())));
        }
    }

    tokio::fs::rename(&tmp, target).await.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::Io(format!("{}: {e}", display_path))
    })?;
    Ok(())
}

/// Deletes a file without ever following a final symlink (US-010, `apply_patch`
/// `*** Delete File`). Symmetric with [`replace_file_confined`]: the same
/// guardrails, one fewer write. A directory is refused, because a patch names
/// files and deleting a tree is not something a stale context should be able to
/// cause.
pub async fn remove_file_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    ensure_existing_path_no_links(workspace, target, display_path)?;
    let meta = std::fs::symlink_metadata(target)
        .map_err(|e| ToolError::Io(format!("{display_path}: {e}")))?;
    if is_link_like(&meta) {
        return Err(ToolError::OutsideWorkspace(display_path.into()));
    }
    if meta.is_dir() {
        return Err(ToolError::Rejected(format!(
            "{display_path} is a directory, not a file"
        )));
    }
    tokio::fs::remove_file(target)
        .await
        .map_err(|e| ToolError::Io(format!("{display_path}: {e}")))
}

fn walk_existing_components(
    workspace: &Path,
    target: &Path,
    display_path: &str,
    allow_missing_leaf: bool,
) -> Result<(), ToolError> {
    let root_lex = lexical_normalize(workspace);
    let root_real =
        std::fs::canonicalize(workspace).map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
    let rel = target
        .strip_prefix(&root_lex)
        .map_err(|_| ToolError::OutsideWorkspace(display_path.into()))?;
    let mut probe = root_real.clone();
    let components: Vec<_> = rel.components().collect();
    for (idx, comp) in components.iter().enumerate() {
        probe.push(comp.as_os_str());
        match std::fs::symlink_metadata(&probe) {
            Ok(meta) => {
                if is_link_like(&meta) {
                    return Err(ToolError::OutsideWorkspace(display_path.into()));
                }
                let real = std::fs::canonicalize(&probe)
                    .map_err(|e| ToolError::Io(format!("{}: {e}", probe.display())))?;
                if !real.starts_with(&root_real) {
                    return Err(ToolError::OutsideWorkspace(display_path.into()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let missing_leaf = idx + 1 == components.len();
                if allow_missing_leaf && missing_leaf {
                    return Ok(());
                }
                return Err(ToolError::Io(format!("{}: {e}", probe.display())));
            }
            Err(e) => return Err(ToolError::Io(format!("{}: {e}", probe.display()))),
        }
    }
    Ok(())
}

fn is_link_like(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Normalizes an absolute path lexically (resolves `.`/`..`).
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_stays_in_workspace() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/main.rs"));
    }

    #[test]
    fn dotdot_escape_is_rejected() {
        let ws = Path::new("/work/repo");
        assert!(matches!(
            confine(ws, "../secret.txt"),
            Err(ToolError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            confine(ws, "src/../../etc/passwd"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn absolute_path_outside_is_rejected() {
        let ws = Path::new("/work/repo");
        assert!(matches!(
            confine(ws, "/etc/passwd"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn absolute_path_inside_is_accepted() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "/work/repo/src/lib.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/lib.rs"));
    }

    #[test]
    fn interior_dotdot_that_stays_inside_is_ok() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "src/foo/../bar.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/bar.rs"));
    }

    fn protected_ws(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pyxis-protected-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        match std::fs::canonicalize(&dir) {
            Ok(real) => real,
            Err(_) => dir,
        }
    }

    #[test]
    fn protected_subpaths_are_refused_for_writes() {
        let ws = protected_ws("direct");
        for path in [
            ".git/hooks/pre-commit",
            ".git/hooks",
            ".git/config",
            ".pyxis/settings.toml",
            // relative climb: `confine` normalizes, the zone is still reached.
            "src/../.git/hooks/post-merge",
            // git worktree: `.git` is a `gitdir: ...` file, rewriting it moves
            // hooks and config to a directory chosen by the attacker.
            ".git",
            ".git/info/exclude",
        ] {
            let err = guard_protected_path(&ws, path).unwrap_err().to_string();
            assert!(
                err.contains("protected path"),
                "{path} doit être refusé comme zone protégée: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn ordinary_workspace_paths_stay_writable() {
        let ws = protected_ws("ordinary");
        for path in [
            "src/main.rs",
            ".github/workflows/ci.yml",
            // close but distinct: the prefix is compared component-wise, so
            // `.gitignore` is not `.git`.
            ".gitignore",
            ".gitmodules",
            ".pyxis-notes.md",
        ] {
            assert!(
                guard_protected_path(&ws, path).is_ok(),
                "{path} ne doit pas être refusé"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn missing_git_directory_is_not_an_error() {
        // AC5: a project without a git repository is not an error case.
        let ws = std::env::temp_dir().join(format!("pyxis-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        assert!(guard_protected_path(&ws, "src/main.rs").is_ok());
        assert!(guard_protected_path(&ws, ".git/hooks/pre-commit").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ───────── US-001: the read-only variant refuses and names itself ─────────

    #[test]
    fn read_only_policy_refuses_every_write_and_names_the_variant() {
        let ws = protected_ws("read-only");
        let policy = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        for path in ["src/main.rs", "README.md", ".gitignore"] {
            let err = guard_write_target(&policy, &ws, path)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("read-only"),
                "{path} doit être refusé en nommant la variante: {err}"
            );
        }
        // Defence in depth: the call-time check refuses too.
        assert!(
            ensure_policy_allows_write(&policy, &ws, &ws.join("src/main.rs"), "src/main.rs")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn full_access_policy_refuses_nothing_not_even_a_protected_subpath() {
        let ws = protected_ws("full-access");
        assert!(
            guard_write_target(
                &SandboxPolicy::DangerFullAccess,
                &ws,
                ".git/hooks/pre-commit"
            )
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ───────── US-002: a shell command cannot reach a protected subpath ─────────

    fn command_policy(ws: &Path) -> SandboxPolicy {
        default_policy(ws)
    }

    #[test]
    fn a_command_writing_into_a_protected_subpath_is_refused_before_execution() {
        // US-002 AC2/AC3: closes the hole `docs/CURRENT_STATUS.md` documented.
        let ws = protected_ws("command-write");
        let policy = command_policy(&ws);
        for command in [
            "cp payload .git/hooks/pre-commit",
            "install -m 755 x .git/hooks/post-merge",
            // The working directory of a shell command is not knowable: this
            // climbs out of the workspace lexically and lands back inside it at
            // run time.
            "cd src && echo evil > ../.git/hooks/pre-commit",
            "cd src; cp payload ../../repo/.pyxis/config.toml",
            "sed -i s/a/b/ .pyxis/config.toml",
            "rm -rf .git",
            // opaque: redirection, substitution, chaining. No argv to read, so
            // the raw text is split and every fragment is tested.
            "echo evil > .git/hooks/pre-commit",
            "cat payload >> .pyxis/config.toml",
            "true && printf x > .git/config",
            "tee .pyxis/config.toml < payload",
            // a valued flag carries its path in the same token.
            "rsync --exclude=x --log-file=.git/log a b",
        ] {
            let err = guard_command_paths(&policy, &ws, command)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("protected path"),
                "{command} doit être refusé avant exécution: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn ordinary_commands_stay_allowed() {
        let ws = protected_ws("command-ok");
        let policy = command_policy(&ws);
        for command in [
            "cargo test --workspace",
            "npm run build",
            "rm -rf target",
            "cp a.txt b.txt",
            "echo x > src/generated.rs",
            // proven side-effect free: reading `.git` is not writing it, and
            // that exemption is what keeps the guard usable.
            "cat .git/config",
            "git log --oneline",
            "ls .git/hooks",
            // close but distinct: the comparison is component-wise.
            "sed -i s/a/b/ .gitignore",
            "cp x .gitmodules",
        ] {
            assert!(
                guard_command_paths(&policy, &ws, command).is_ok(),
                "{command} ne doit pas être refusé"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_reaching_a_protected_subpath_through_a_symlink_is_refused() {
        // US-002 AC4: resolution precedes the decision, as it already does for
        // the editing tools.
        let ws = protected_ws("command-symlink");
        let link = ws.join("hooks-link");
        if std::os::unix::fs::symlink(ws.join(".git/hooks"), &link).is_err() {
            let _ = std::fs::remove_dir_all(&ws);
            return;
        }
        let policy = command_policy(&ws);
        let err = guard_command_paths(&policy, &ws, "cp payload hooks-link/pre-commit")
            .unwrap_err()
            .to_string();
        assert!(err.contains("protected path"), "{err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_policy_granting_no_root_leaves_the_decision_to_the_kernel() {
        // Read-only and full access subtract nothing: there is no writable root
        // to take a subpath out of, so this guard has no claim to make.
        let ws = protected_ws("command-no-root");
        for policy in [
            SandboxPolicy::DangerFullAccess,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        ] {
            assert!(guard_command_paths(&policy, &ws, "cp x .git/hooks/pre-commit").is_ok());
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn evaluating_a_command_costs_well_under_a_millisecond() {
        // US-002 AC6: the guard runs on the permission path of every `bash`
        // call, so its cost is a budget, not a detail.
        let ws = protected_ws("command-perf");
        let policy = command_policy(&ws);
        // Representative of a real call: several path arguments, one of them
        // opaque enough to force the fragment split.
        let command = "cargo test --workspace --manifest-path ./Cargo.toml --target-dir target/debug 2> build.log";
        const ROUNDS: u32 = 200;
        // Warm-up: the first canonicalize pays for the cold dentry cache.
        for _ in 0..20 {
            let _ = guard_command_paths(&policy, &ws, command);
        }
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let _ = guard_command_paths(&policy, &ws, command);
        }
        let per_call = start.elapsed() / ROUNDS;
        assert!(
            per_call < std::time::Duration::from_millis(1),
            "surcoût par appel: {per_call:?} (budget: 1 ms)"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_protected_zone_is_refused_like_the_direct_path() {
        let ws = protected_ws("symlink");
        let link = ws.join("hooks-link");
        if std::os::unix::fs::symlink(ws.join(".git/hooks"), &link).is_err() {
            let _ = std::fs::remove_dir_all(&ws);
            return;
        }
        let err = guard_protected_path(&ws, "hooks-link/pre-commit")
            .unwrap_err()
            .to_string();
        assert!(err.contains("protected path"), "{err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn real_path_rejects_symlink_escape_when_platform_allows_symlink() {
        let root = std::env::temp_dir().join(format!("pyxis-path-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("pyxis-path-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("link");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();
        if !linked {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let err = ensure_existing_ancestor_confined(&root, &link.join("file.txt"), "link/file.txt")
            .unwrap_err();
        assert!(matches!(err, ToolError::OutsideWorkspace(_)));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
