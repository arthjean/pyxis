//! Confinement policy: the data model of the perimeter (US-001, US-002).
//!
//! PURE DATA, no I/O (invariant ADR-3): `agent-core` describes the perimeter,
//! `agent-sandbox` applies it at kernel level and `agent-tools` evaluates it
//! before running anything. Putting the type here is what lets those two crates
//! share it without `agent-tools` gaining a dependency on `agent-sandbox`.
//!
//! Two levels answer two different questions:
//! - which roots the KERNEL grants (Landlock rules are additive, so a granted
//!   right can never be subtracted afterwards);
//! - which paths the POLICY allows to write ([`WritableRoot::is_path_writable`]),
//!   evaluated before execution, which is the only level able to subtract a
//!   subpath from a root the kernel already opened. That asymmetry is the whole
//!   reason `read_only_subpaths` exists.

use std::path::{Component, Path, PathBuf};

/// Identifier of the full-access variant, as accepted in configuration.
pub const FULL_ACCESS_ID: &str = "full-access";
/// Identifier of the read-only variant.
pub const READ_ONLY_ID: &str = "read-only";
/// Identifier of the workspace-write variant.
pub const WORKSPACE_WRITE_ID: &str = "workspace-write";

/// A root granted for writing, and the subpaths subtracted from it.
///
/// `read_only_subpaths` are RELATIVE to `root`: the same declaration then
/// describes any checkout, and the comparison stays component-wise (`.gitignore`
/// is never a match for `.git`).
///
/// Every method here expects an ALREADY NORMALIZED absolute path.
/// [`SandboxPolicy`] is the normalizing entry point: it resolves `.`/`..` once
/// and then queries each root, instead of paying for it per root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    pub read_only_subpaths: Vec<PathBuf>,
}

impl WritableRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: normalize(&root.into()),
            read_only_subpaths: Vec::new(),
        }
    }

    /// Subtracts subpaths from this root. Declared relative to the root.
    pub fn with_read_only_subpaths<I, P>(mut self, subpaths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.read_only_subpaths = subpaths.into_iter().map(Into::into).collect();
        self
    }

    /// Is `path` under this root at all? Lexical: the caller resolves symlinks
    /// when it needs the real path (a policy decision must never depend on a
    /// disk access it can be denied).
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// The read-only zone that covers `path`, or `None` when the path is
    /// writable under this root. Returns the zone so the refusal can name it.
    pub fn read_only_zone(&self, path: &Path) -> Option<&Path> {
        let rel = path.strip_prefix(&self.root).ok()?;
        self.read_only_subpaths
            .iter()
            .find(|zone| rel.starts_with(zone))
            .map(PathBuf::as_path)
    }

    /// Ported from Codex `WritableRoot::is_path_writable`
    /// (`codex-rs/protocol/src/protocol.rs:1050-1078`), adapted: subpaths are
    /// relative here, and the metadata-name rule is not ported (Pyxis protects
    /// `.git` as a whole, which subsumes it).
    pub fn is_path_writable(&self, path: &Path) -> bool {
        self.contains(path) && self.read_only_zone(path).is_none()
    }
}

/// Confinement policy. Each variant carries its own data: that is what makes
/// read-only, network control and subtracted subpaths expressible at all, where
/// a boolean could only say "sandbox on/off".
///
/// Ported from Codex `SandboxPolicy` (`codex-rs/protocol/src/protocol.rs:995`).
/// `ExternalSandbox` is deliberately not ported: Pyxis never delegates
/// confinement to a surrounding runtime, so the variant would have no consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// No confinement at all. What `--no-sandbox` selects.
    DangerFullAccess,
    /// Nothing is writable. The agent reads and runs, it does not mutate.
    ReadOnly { network_access: bool },
    /// Writes confined to the declared roots, minus their read-only subpaths.
    WorkspaceWrite {
        /// The workspace comes FIRST: it is the root the kernel anchors on and
        /// the only one carrying subtracted subpaths.
        writable_roots: Vec<WritableRoot>,
        network_access: bool,
    },
}

impl SandboxPolicy {
    /// The default policy: writes confined to `workspace` plus `extra_roots`,
    /// with `read_only_subpaths` subtracted from the workspace alone. Network
    /// left to the allow-list, which is the behavior that predates US-001.
    pub fn workspace_write<R, S, P>(
        workspace: impl Into<PathBuf>,
        extra_roots: R,
        subpaths: S,
    ) -> Self
    where
        R: IntoIterator<Item = PathBuf>,
        S: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let mut writable_roots =
            vec![WritableRoot::new(workspace).with_read_only_subpaths(subpaths)];
        writable_roots.extend(extra_roots.into_iter().map(WritableRoot::new));
        Self::WorkspaceWrite {
            writable_roots,
            network_access: true,
        }
    }

    /// Parses a configuration or command-line value. `None` names an unknown
    /// value rather than falling back on a default, so the caller can list the
    /// accepted ones.
    pub fn from_id(
        id: &str,
        workspace: &Path,
        extra_roots: Vec<PathBuf>,
        subpaths: &[&str],
    ) -> Option<Self> {
        match id.trim() {
            FULL_ACCESS_ID => Some(Self::DangerFullAccess),
            READ_ONLY_ID => Some(Self::ReadOnly {
                network_access: false,
            }),
            WORKSPACE_WRITE_ID => Some(Self::workspace_write(
                workspace,
                extra_roots,
                subpaths.iter().map(PathBuf::from),
            )),
            _ => None,
        }
    }

    /// The accepted identifiers, for an error message that lists them.
    pub const IDS: &'static [&'static str] = &[FULL_ACCESS_ID, READ_ONLY_ID, WORKSPACE_WRITE_ID];

    /// Stable identifier, the same spelling configuration uses.
    pub fn id(&self) -> &'static str {
        match self {
            Self::DangerFullAccess => FULL_ACCESS_ID,
            Self::ReadOnly { .. } => READ_ONLY_ID,
            Self::WorkspaceWrite { .. } => WORKSPACE_WRITE_ID,
        }
    }

    /// Does the policy allow the tools to reach the network at all? When false
    /// the allow-list is moot: nothing passes.
    pub fn network_access(&self) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly { network_access } | Self::WorkspaceWrite { network_access, .. } => {
                *network_access
            }
        }
    }

    /// Roots the policy grants for writing. Empty under read-only.
    pub fn writable_roots(&self) -> &[WritableRoot] {
        match self {
            Self::DangerFullAccess | Self::ReadOnly { .. } => &[],
            Self::WorkspaceWrite { writable_roots, .. } => writable_roots,
        }
    }

    /// Does any variant confine writes at all?
    pub fn confines_writes(&self) -> bool {
        !matches!(self, Self::DangerFullAccess)
    }

    /// The read-only subpath covering `path`, `None` when no root subtracts it.
    ///
    /// Deliberately independent of whether the policy allows writes at all: this
    /// is the SUBTRACTION level, and it is what a shell command must be tested
    /// against (US-002), since a command is never proven to write.
    pub fn read_only_zone(&self, path: &Path) -> Option<PathBuf> {
        let normalized = normalize(path);
        self.writable_roots()
            .iter()
            .find_map(|root| root.read_only_zone(&normalized).map(Path::to_path_buf))
    }

    /// Why `path` cannot be written under this policy, `None` when it can.
    /// Full access allows everything; read-only refuses everything and names
    /// itself; workspace-write defers to the roots.
    pub fn write_refusal(&self, path: &Path) -> Option<WriteRefusal> {
        match self {
            Self::DangerFullAccess => None,
            Self::ReadOnly { .. } => Some(WriteRefusal::ReadOnlyPolicy),
            Self::WorkspaceWrite { .. } => {
                self.read_only_zone(path).map(WriteRefusal::ReadOnlySubpath)
            }
        }
    }
}

/// Why the policy refuses a write. Carries what the message must name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    /// The whole policy forbids writing.
    ReadOnlyPolicy,
    /// A subpath subtracted from an otherwise writable root.
    ReadOnlySubpath(PathBuf),
}

impl std::fmt::Display for WriteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnlyPolicy => {
                write!(f, "the `{READ_ONLY_ID}` sandbox policy forbids every write")
            }
            Self::ReadOnlySubpath(zone) => write!(
                f,
                "`{}` is a read-only subpath of a writable root",
                zone.display()
            ),
        }
    }
}

/// Lexical normalization (`.`/`..` resolved without touching the disk), so a
/// policy decision never depends on a path existing.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
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

    fn policy() -> SandboxPolicy {
        SandboxPolicy::workspace_write(
            "/work/repo",
            vec![PathBuf::from("/tmp/scratch")],
            [".git", ".pyxis"],
        )
    }

    #[test]
    fn every_variant_carries_its_own_data() {
        // US-001 AC1: three variants, each answering with its own perimeter.
        assert_eq!(SandboxPolicy::DangerFullAccess.id(), FULL_ACCESS_ID);
        assert!(SandboxPolicy::DangerFullAccess.network_access());
        assert!(!SandboxPolicy::DangerFullAccess.confines_writes());

        let read_only = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        assert_eq!(read_only.id(), READ_ONLY_ID);
        assert!(!read_only.network_access());
        assert!(read_only.writable_roots().is_empty());

        let ws = policy();
        assert_eq!(ws.id(), WORKSPACE_WRITE_ID);
        assert_eq!(ws.writable_roots().len(), 2);
    }

    #[test]
    fn read_only_refuses_every_write_and_names_itself() {
        // US-001 AC2: the refusal names the variant in cause.
        let read_only = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let refusal = read_only
            .write_refusal(Path::new("/work/repo/src/main.rs"))
            .expect("read-only refuses");
        assert_eq!(refusal, WriteRefusal::ReadOnlyPolicy);
        assert!(refusal.to_string().contains(READ_ONLY_ID));
    }

    #[test]
    fn full_access_refuses_nothing() {
        assert!(
            SandboxPolicy::DangerFullAccess
                .write_refusal(Path::new("/etc/passwd"))
                .is_none()
        );
    }

    #[test]
    fn workspace_write_subtracts_declared_subpaths() {
        // US-002 AC1: the root is writable, its declared subpaths are not.
        let ws = policy();
        assert!(
            ws.write_refusal(Path::new("/work/repo/src/main.rs"))
                .is_none()
        );
        assert!(ws.write_refusal(Path::new("/tmp/scratch/x")).is_none());
        // A neighbour whose name merely shares a prefix stays writable: the
        // comparison is component-wise.
        assert!(
            ws.write_refusal(Path::new("/work/repo/.gitignore"))
                .is_none()
        );
        for refused in [
            "/work/repo/.git",
            "/work/repo/.git/hooks/pre-commit",
            "/work/repo/.pyxis/config.toml",
            // `..` is resolved before the comparison, so a climb cannot hide the zone.
            "/work/repo/src/../.git/config",
        ] {
            let refusal = ws.write_refusal(Path::new(refused));
            assert!(
                matches!(refusal, Some(WriteRefusal::ReadOnlySubpath(_))),
                "{refused} must be refused: {refusal:?}"
            );
        }
    }

    #[test]
    fn a_path_outside_every_root_is_not_the_policys_business() {
        // Outside the granted roots, the kernel is the authority, not this
        // lexical test: reporting a refusal here would name the wrong cause.
        assert!(policy().write_refusal(Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn subpaths_belong_to_the_root_that_declares_them() {
        let ws = policy();
        // `.git` is subtracted from the workspace, not from the extra root.
        assert!(
            ws.write_refusal(Path::new("/tmp/scratch/.git/config"))
                .is_none()
        );
    }

    #[test]
    fn ids_round_trip_through_configuration() {
        for id in SandboxPolicy::IDS {
            let parsed = SandboxPolicy::from_id(id, Path::new("/work/repo"), Vec::new(), &[".git"]);
            assert_eq!(parsed.as_ref().map(SandboxPolicy::id), Some(*id));
        }
        assert!(SandboxPolicy::from_id("nope", Path::new("/w"), Vec::new(), &[]).is_none());
    }

    #[test]
    fn is_path_writable_is_the_root_level_primitive() {
        let root = WritableRoot::new("/work/repo").with_read_only_subpaths([".git", ".pyxis"]);
        assert!(root.is_path_writable(Path::new("/work/repo/src/lib.rs")));
        assert!(!root.is_path_writable(Path::new("/work/repo/.git/config")));
        // Outside the root: not writable *under this root*.
        assert!(!root.is_path_writable(Path::new("/elsewhere/x")));
    }
}
