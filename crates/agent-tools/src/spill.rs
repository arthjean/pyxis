//! Spill storage (US-070/US-071): the one place able to write a full tool
//! output to disk so that bounding a result stops meaning destroying it.
//!
//! The module does exactly one thing: persist a text and hand back a locator.
//! It decides nothing about WHEN a spill happens, nothing about what the model
//! reads instead, and nothing about retention. A real write failure surfaces as
//! a typed error; degrading to "no spill" is the caller's call, never this
//! module's.
//!
//! **No trait, on purpose.** One implementation exists and no second storage is
//! required; the abstraction stays refused until a remote or database backend
//! actually exists. A concrete struct is what the current requirement forces.
//!
//! The root is `<workspace>/.pyxis/spill/<short thread hash>` rather than a
//! private directory under the system temp dir. `confine` (`path.rs`) refuses
//! `read` and `grep` any path outside the workspace, so a spill outside it
//! would produce an artifact the model can neither read nor search. The
//! unpredictability lost that way is replaced by a stronger property Pyxis
//! already owns: `.pyxis` is in `PROTECTED_SUBPATHS`, so no tool, `bash`
//! included, can write there and plant a symlink in the root.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rand::RngCore;
use sha2::{Digest, Sha256};

/// Workspace-relative parent of every per-thread spill directory. Under
/// [`PYXIS_DIR`] so the protected-subpath rule applies to it for free.
pub const SPILL_DIR: &str = "spill";

/// The protected directory the spill root hangs under. It is also listed in
/// [`crate::path::PROTECTED_SUBPATHS`], which is what makes the root unwritable
/// by every tool including `bash`; a test pins the two together so moving one
/// without the other fails.
pub const PYXIS_DIR: &str = ".pyxis";

/// Hex characters kept from the thread identifier hash. Long enough that two
/// live threads do not collide, short enough that the locator stays cheap in
/// the model's context.
const THREAD_HASH_HEX: usize = 12;

/// Random bytes prefixed to every file name. Unpredictable enough that a name
/// cannot be pre-created by anything that can guess the tool name, while the
/// readable half stays readable.
const NAME_PREFIX_BYTES: usize = 6;

/// Bytes a single path component may occupy on Linux (`NAME_MAX`). The whole
/// file name is bounded BEFORE the syscall so the failure is a deterministic
/// truncation and not an `ENAMETOOLONG` at write time.
const MAX_FILE_NAME_BYTES: usize = 255;

/// Cumulative size `.pyxis/spill/` may reach, every thread directory of the
/// workspace included, before the oldest of them are evicted.
///
/// The unit of the problem is one oversized output, and the PRD sizes it at
/// 10 MiB (a full build log). 256 MiB therefore holds about twenty-five of
/// them, far more than one working session produces, while staying an order of
/// magnitude below the `target/` directory the same workspace already carries:
/// the bound trips long before the disk notices, and never inside the session
/// that is producing the artifacts. A crate constant and not a configuration
/// key, invariant 15.
pub const MAX_SPILL_ROOT_BYTES: u64 = 256 * 1024 * 1024;

/// Owner-only directory and file modes: another local user must not be able to
/// read spilled tool output (CWE-377).
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Bytes one escaped code point occupies: `~` plus six uppercase hex digits.
/// Fixed width is what makes [`encode_segment`] injective, since a code point
/// reaches `0x10FFFF`.
const ESCAPE_LEN: usize = 7;

/// Why a spill could not be written. The module never degrades on its own: it
/// names the failure and lets the caller decide (a spill failure must never
/// turn a successful tool call into an error).
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    #[error("spill directory {path}: {source}")]
    Directory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("spill file {path}: {source}")]
    File {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// One written spill artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    /// Model-facing handle: a path RELATIVE to the workspace. Relative because
    /// `read` already resolves relative paths against the workspace, because no
    /// absolute path then leaks into the JSONL stream or to the app-server, and
    /// because a shorter notice leaves more budget to the preview.
    pub locator: String,
    /// Bytes actually written.
    pub bytes: usize,
}

/// A spill file open while its content is still being produced.
///
/// Nothing buffers in front of the file: every [`write`](Self::write) reaches
/// the disk. The file must match what was actually read at any instant,
/// including when a cancelled turn drops the writer between two reads, and a
/// buffer dropped on that path would silently lose its last kilobytes.
#[derive(Debug)]
pub struct SpillWriter {
    file: std::fs::File,
    /// Kept for the error message only: the locator is what the caller uses.
    path: PathBuf,
    locator: String,
    bytes: usize,
}

impl SpillWriter {
    /// Appends `chunk`. An error leaves the caller to decide the degradation,
    /// exactly like [`SpillStore::save`]: this type never absorbs a failure.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), SpillError> {
        self.file
            .write_all(chunk)
            .map_err(|source| SpillError::File {
                path: self.path.display().to_string(),
                source,
            })?;
        self.bytes += chunk.len();
        Ok(())
    }

    /// Closes the file and reports what was written.
    pub fn finish(self) -> SpillRef {
        SpillRef {
            locator: self.locator,
            bytes: self.bytes,
        }
    }
}

/// The storage itself: a resolved root, and the ability to write into it.
#[derive(Debug)]
pub struct SpillStore {
    root: PathBuf,
    /// `.pyxis/spill/<hash>`, built rather than derived from `root`, so the
    /// locator is relative by construction and not by a fallible strip.
    locator_prefix: String,
}

impl SpillStore {
    /// Creates the per-thread root (owner-only) and returns the store.
    ///
    /// Called by the binary BEFORE the sandbox is enforced: a Landlock rule
    /// needs a path that already opens, exactly like the session directory.
    /// The directory is named by a short hash of `thread_id` and never by the
    /// identifier itself, because the locator travels into the model's context
    /// and into the JSONL stream.
    pub fn create(workspace: &Path, thread_id: &str) -> Result<Self, SpillError> {
        let name = thread_dir_name(thread_id);
        let locator_prefix = format!("{PYXIS_DIR}/{SPILL_DIR}/{name}");
        let parent = workspace.join(PYXIS_DIR).join(SPILL_DIR);
        let root = parent.join(&name);
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&root)
            .map_err(|source| SpillError::Directory {
                path: root.display().to_string(),
                source,
            })?;
        // US-081: the sweep happens HERE, once, right after the directory of
        // the starting thread exists and before anything writes into it. It
        // never fails the caller: a spill root that cannot be swept is a disk
        // that grows, not a thread that refuses to start.
        evict_over_cap(&parent, &root, MAX_SPILL_ROOT_BYTES);
        Ok(Self {
            root,
            locator_prefix,
        })
    }

    /// The absolute root, for the caller that must hand it to the sandbox.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes `content` whole into a fresh file and returns its locator.
    ///
    /// `tool_name` and `call_id` are purely descriptive: they make the file
    /// name readable and inspectable, and are never interpreted for access
    /// control. Both are untrusted (an MCP server names its own tools), hence
    /// the injective encoding before any filesystem use.
    ///
    /// The open is exclusive and owner-only: it fails on ANY existing path,
    /// symlink included, so a pre-planted target cannot redirect the write
    /// (CWE-59, CWE-367). No retry on collision: a taken name is reported as a
    /// write failure, which the caller already has to handle.
    pub fn save(
        &self,
        tool_name: &str,
        call_id: &str,
        content: &str,
    ) -> Result<SpillRef, SpillError> {
        let mut writer = self.open(tool_name, call_id)?;
        writer.write(content.as_bytes())?;
        Ok(writer.finish())
    }

    /// Creates the same file [`save`](Self::save) creates, but hands it back
    /// open so bytes can be written as they arrive.
    ///
    /// A producer whose stream is bounded AS IT IS READ never holds its own
    /// output: `bash` drops the head of a chatty command minutes before the
    /// command ends, so there is no `content` to hand to `save`. The creation
    /// rules are the same one for one, exclusive open included; only the moment
    /// the bytes arrive differs.
    pub fn open(&self, tool_name: &str, call_id: &str) -> Result<SpillWriter, SpillError> {
        let file_name = self.file_name(tool_name, call_id);
        let path = self.root.join(&file_name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
            .map_err(|source| SpillError::File {
                path: path.display().to_string(),
                source,
            })?;
        Ok(SpillWriter {
            file,
            path,
            locator: format!("{}/{}", self.locator_prefix, file_name),
            bytes: 0,
        })
    }

    /// `<random hex>-<encoded name>`, bounded to what one path component holds.
    fn file_name(&self, tool_name: &str, call_id: &str) -> String {
        let mut raw = [0u8; NAME_PREFIX_BYTES];
        rand::rng().fill_bytes(&mut raw);
        let prefix = hex::encode(raw);
        let suggested = format!("{tool_name}-{call_id}.txt");
        // The prefix and its separator are the fixed cost; whatever is left of
        // `NAME_MAX` goes to the readable half.
        let budget = MAX_FILE_NAME_BYTES - prefix.len() - 1;
        let encoded = encode_segment(&suggested);
        format!("{prefix}-{}", bound_segment(&encoded, budget))
    }
}

/// One thread directory as the sweep sees it.
struct ThreadDir {
    path: PathBuf,
    bytes: u64,
    /// Last modification of the DIRECTORY, which a spill file created inside it
    /// bumps: the age of a thread is the age of its last artifact, not of its
    /// first. An unreadable timestamp reads as the epoch, so a directory
    /// nothing can date is evicted first rather than kept forever.
    modified: std::time::SystemTime,
}

/// Brings the spill root back under `cap` by deleting whole thread directories,
/// oldest first.
///
/// **Per thread, never per file.** A resumed session finds the artifacts of a
/// thread whole or absent, and a locator that no longer resolves fails as a
/// plain missing file, which is readable. Evicting file by file would leave a
/// thread half readable, which nothing in the transcript could explain.
///
/// `current` is never a candidate: a run must not evict the directory it is
/// about to write locators into.
fn evict_over_cap(parent: &Path, current: &Path, cap: u64) {
    let mut dirs = thread_dirs(parent);
    let mut total: u64 = dirs.iter().map(|dir| dir.bytes).sum();
    if total <= cap {
        return;
    }
    dirs.sort_by_key(|dir| dir.modified);
    for dir in dirs {
        if total <= cap {
            break;
        }
        if dir.path == current {
            continue;
        }
        match remove_under(parent, &dir.path) {
            Ok(()) => {
                total = total.saturating_sub(dir.bytes);
                // Information and not debug: a spilled file a transcript still
                // references disappears here, and a silent deletion would make
                // the missing-file error it later produces undiagnosable.
                tracing::info!(
                    target: "pyxis::tools",
                    dir = %dir.path.display(),
                    bytes = dir.bytes,
                    remaining = total,
                    cap,
                    "spill root over cap: evicted the oldest thread directory"
                );
            }
            Err(error) => tracing::warn!(
                target: "pyxis::tools",
                dir = %dir.path.display(),
                error = %error,
                "spill root over cap: eviction failed, the root stays over its bound"
            ),
        }
    }
}

/// The thread directories of `parent`, with their size and their age.
///
/// The size is the sum of the DIRECT file entries: the store writes a flat
/// directory per thread and nothing else can write under `.pyxis`, so a
/// recursive walk would cost a syscall per entry to sum the same bytes. An
/// entry whose metadata cannot be read counts as zero rather than aborting the
/// sweep, since undercounting only delays an eviction.
fn thread_dirs(parent: &Path) -> Vec<ThreadDir> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let path = entry.path();
            ThreadDir {
                bytes: directory_bytes(&path),
                modified: entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
                path,
            }
        })
        .collect()
}

fn directory_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Deletes `candidate`, and refuses BEFORE the syscall anything that is not a
/// direct child of `root`.
///
/// The check is not defensive decoration: `remove_dir_all` is the one
/// destructive call of the whole spill subsystem, and the only guarantee worth
/// stating about it is that no argument outside `.pyxis/spill/` ever reaches
/// it. A direct child is the exact shape the sweep produces, so a candidate
/// that climbs out with `..`, names the root itself, or points deeper than one
/// component is a bug and is reported as one.
fn remove_under(root: &Path, candidate: &Path) -> std::io::Result<()> {
    let inside = candidate.parent() == Some(root)
        && candidate
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir));
    if !inside {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a thread directory of {}",
                candidate.display(),
                root.display()
            ),
        ));
    }
    std::fs::remove_dir_all(candidate)
}

/// A fresh opaque identifier for one run of the binary, 128 random bits.
///
/// The runtime's `ThreadId` would be the natural owner of a spill root, but it
/// is minted by `SessionRuntime` LONG after `enforce_sandbox`, and a Landlock
/// rule needs a path that already opens. The binary therefore draws its own run
/// identifier before the sandbox, and [`SpillStore::create`] hashes it: the
/// grouping property (one directory per run, evictable as a whole) is the one
/// US-081 needs, and the identifier still never appears in clear on a path.
pub fn run_id() -> String {
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

/// `<12 hex of sha256(thread_id)>`: groups artifacts per thread without ever
/// putting the identifier itself on a path the model reads.
pub fn thread_dir_name(thread_id: &str) -> String {
    let digest = Sha256::digest(thread_id.as_bytes());
    hex::encode(digest)[..THREAD_HASH_HEX].to_string()
}

/// Encodes an arbitrary string as ONE path segment, injectively over every
/// Rust string.
///
/// A tool name comes from an MCP server and a call id from the provider, so
/// both are untrusted input: this neutralizes `../`, absolute paths, NUL and
/// separators before any filesystem use (CWE-22). Each code point is kept
/// literal (`[A-Za-z0-9._-]`, minus `~`) or escaped as `~XXXXXX`; `~` is itself
/// escaped, so the mapping is reversible and two distinct inputs never collide.
/// The whole-segment tokens `.` and `..` are escaped entirely so they can never
/// traverse, and the empty string encodes to `~`, never to an empty segment.
///
/// Six hex digits, not four: Rust encodes by code point (up to `0x10FFFF`), so
/// a four-digit form would be ambiguous and would lose injectivity.
pub fn encode_segment(raw: &str) -> String {
    match raw {
        "" => return "~".to_string(),
        "." => return escape('.'),
        ".." => return format!("{}{}", escape('.'), escape('.')),
        _ => {}
    }
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch != '~' && (ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
            out.push(ch);
        } else {
            out.push_str(&escape(ch));
        }
    }
    out
}

fn escape(ch: char) -> String {
    format!("~{:06X}", ch as u32)
}

/// Truncates an encoded segment to `max` bytes on a token boundary.
///
/// Deterministic by construction: the encoding is ASCII made of one-byte
/// literals and fixed-width escapes, so cutting between tokens can never split
/// an escape nor produce invalid UTF-8. Truncation is applied to the FILE NAME,
/// never inside [`encode_segment`], which stays injective.
fn bound_segment(encoded: &str, max: usize) -> &str {
    if encoded.len() <= max {
        return encoded;
    }
    let bytes = encoded.as_bytes();
    let mut end = 0;
    while end < bytes.len() {
        let step = if bytes[end] == b'~' { ESCAPE_LEN } else { 1 };
        if end + step > max {
            break;
        }
        end += step;
    }
    &encoded[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Scoped workspace, same shape as `turn_diff`'s test repo: process id plus
    /// a counter, so two tests of the same run never share a directory.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("pyxis-spill-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            // Restore the write bit the permission test removes, otherwise the
            // recursive delete leaves the tree behind.
            if let Ok(meta) = std::fs::metadata(&self.0) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(&self.0, perms);
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_spill_root_lives_under_a_protected_subpath() {
        assert!(
            crate::path::PROTECTED_SUBPATHS.contains(&PYXIS_DIR),
            "the spill root must stay unwritable by every tool"
        );
    }

    /// US-072 AC3: the default is ABSENCE, which means no spill at all rather
    /// than an implicit root, so every caller outside the binary keeps its
    /// pre-EP-022 behavior.
    #[test]
    fn a_tool_context_carries_no_spill_store_by_default() {
        let ctx = crate::tool::ToolCtx::new(std::env::temp_dir());
        assert!(ctx.spill.is_none());
    }

    /// US-072 AC3: and the builder is the one way it becomes present.
    #[test]
    fn the_registry_builder_hands_the_store_to_the_tool_context() {
        let ws = Workspace::new();
        let store = std::sync::Arc::new(SpillStore::create(ws.path(), "thread_1").unwrap());
        let registry = crate::registry::Registry::builder(ws.path())
            .spill(std::sync::Arc::clone(&store))
            .build();
        let wired = registry.ctx.spill.as_ref().expect("a wired store");
        assert_eq!(wired.root(), store.root());
    }

    #[test]
    fn a_saved_text_lands_under_the_workspace_relative_locator() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let saved = store.save("bash", "call_1", "hello").unwrap();

        assert_eq!(saved.bytes, 5);
        assert!(
            !Path::new(&saved.locator).is_absolute(),
            "locator must be relative: {}",
            saved.locator
        );
        assert!(
            saved.locator.starts_with(".pyxis/spill/"),
            "{}",
            saved.locator
        );
        let on_disk = ws.path().join(&saved.locator);
        assert_eq!(std::fs::read_to_string(on_disk).unwrap(), "hello");
    }

    #[test]
    fn a_ten_mebibyte_output_is_written_byte_for_byte() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let content: String = std::iter::repeat_n("line\r\n", 10 * 1024 * 1024 / 6).collect();
        let saved = store.save("bash", "call_1", &content).unwrap();

        assert_eq!(saved.bytes, content.len());
        let read_back = std::fs::read(ws.path().join(&saved.locator)).unwrap();
        assert_eq!(read_back.len(), content.len());
        assert_eq!(
            read_back,
            content.as_bytes(),
            "no re-encoding, no newline normalization"
        );
    }

    #[test]
    fn the_directory_and_the_file_are_owner_only() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let saved = store.save("bash", "call_1", "x").unwrap();

        let dir_mode = std::fs::metadata(store.root())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(ws.path().join(&saved.locator))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, DIR_MODE);
        assert_eq!(file_mode, FILE_MODE);
    }

    #[test]
    fn two_saves_with_identical_arguments_produce_two_distinct_files() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let first = store.save("bash", "call_1", "a").unwrap();
        let second = store.save("bash", "call_1", "b").unwrap();

        assert_ne!(first.locator, second.locator);
        assert_eq!(
            std::fs::read_to_string(ws.path().join(&first.locator)).unwrap(),
            "a"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join(&second.locator)).unwrap(),
            "b"
        );
    }

    #[test]
    fn an_existing_path_is_never_overwritten() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        // The name is unpredictable, so the exclusive open is proved on the
        // path the store itself would use: re-opening it must fail.
        let saved = store.save("bash", "call_1", "first").unwrap();
        let taken = ws.path().join(&saved.locator);
        let err = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&taken)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "first");
    }

    /// US-070, the symlink half of the same criterion: `create_new` refuses a
    /// planted link instead of following it, so a write cannot be redirected
    /// outside the root (CWE-59). Proved on a DANGLING link, the case a plain
    /// existence check would let through.
    #[test]
    fn a_planted_symlink_is_never_followed() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let target = ws.path().join("outside.txt");
        let link = store.root().join("planted");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&link)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!target.exists(), "the link target must never be created");
    }

    #[test]
    fn a_write_failure_surfaces_as_a_typed_error_without_degrading() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        // Owner-only minus write: the directory still opens, the create fails.
        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(store.root(), perms).unwrap();

        let err = store.save("bash", "call_1", "x").unwrap_err();
        assert!(matches!(err, SpillError::File { .. }), "{err:?}");

        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(DIR_MODE);
        std::fs::set_permissions(store.root(), perms).unwrap();
    }

    #[test]
    fn a_root_that_cannot_be_created_surfaces_as_a_typed_error() {
        let ws = Workspace::new();
        // A FILE where `.pyxis` must be a directory: `create_dir_all` fails.
        std::fs::write(ws.path().join(".pyxis"), "not a directory").unwrap();
        let err = SpillStore::create(ws.path(), "thread_1").unwrap_err();
        assert!(matches!(err, SpillError::Directory { .. }), "{err:?}");
    }

    #[test]
    fn the_thread_directory_never_carries_the_identifier_in_clear() {
        let name = thread_dir_name("thr_0123456789abcdef0123456789abcdef");
        assert_eq!(name.len(), THREAD_HASH_HEX);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!name.contains("thr_"));
        assert_eq!(
            name,
            thread_dir_name("thr_0123456789abcdef0123456789abcdef")
        );
        assert_ne!(name, thread_dir_name("thr_1"));
    }

    /// The hostile set of US-071: every entry a tool name could carry that
    /// would otherwise become a path.
    const HOSTILE: &[&str] = &[
        "../../etc/passwd",
        "/etc/passwd",
        "a\0b",
        "a/b",
        "a\\b",
        ".",
        "..",
        "",
        "~",
        "..%2f..",
        "bash",
        "mcp__server__tool",
        "café",
        "\u{1F600}",
    ];

    #[test]
    fn the_segment_encoding_is_injective_over_the_hostile_set() {
        let mut seen = std::collections::BTreeMap::new();
        for raw in HOSTILE {
            let encoded = encode_segment(raw);
            let clash = seen.insert(encoded.clone(), *raw);
            assert!(
                clash.is_none(),
                "`{clash:?}` and `{raw}` both encode to `{encoded}`"
            );
        }
        assert_eq!(seen.len(), HOSTILE.len());
    }

    #[test]
    fn no_hostile_name_escapes_the_root() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        for raw in HOSTILE {
            let saved = store.save(raw, "call_1", "x").unwrap();
            let resolved = ws.path().join(&saved.locator);
            assert!(
                resolved.starts_with(store.root()),
                "`{raw}` resolved to {} outside {}",
                resolved.display(),
                store.root().display()
            );
            assert_eq!(
                resolved.components().count(),
                store.root().components().count() + 1,
                "`{raw}` produced more than one component under the root"
            );
            assert!(resolved.is_file(), "`{raw}` did not produce a file");
        }
    }

    #[test]
    fn the_empty_name_and_the_dot_tokens_never_produce_a_traversing_segment() {
        assert_eq!(encode_segment(""), "~");
        assert_eq!(encode_segment("."), "~00002E");
        assert_eq!(encode_segment(".."), "~00002E~00002E");
        assert_eq!(encode_segment("~"), "~00007E");
        assert_eq!(encode_segment("a.b"), "a.b");
        assert_eq!(encode_segment("a/b"), "a~00002Fb");
    }

    /// The unhappy path of US-071: an over-long name is TRUNCATED, never
    /// refused, and the truncation happens before the syscall.
    #[test]
    fn an_over_long_name_is_truncated_deterministically_before_the_syscall() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        // Every code point escapes to 7 bytes, so 300 of them blow past NAME_MAX.
        let hostile = "/".repeat(300);
        let name = store.file_name(&hostile, "call_1");

        assert!(name.len() <= MAX_FILE_NAME_BYTES, "{}", name.len());
        // Cut on a token boundary: the tail is a whole escape, never a fragment.
        assert!(name.ends_with("~00002F"), "{name}");
        assert_eq!(
            bound_segment(&encode_segment(&hostile), 100),
            bound_segment(&encode_segment(&hostile), 100),
            "the same input always truncates to the same segment"
        );
        let saved = store.save(&hostile, "call_1", "x").unwrap();
        assert!(ws.path().join(&saved.locator).is_file());
    }

    // ---- US-081: the bound on the spill root ----

    /// The parent of every thread directory, which the sweep reads.
    fn spill_parent(ws: &Workspace) -> PathBuf {
        ws.path().join(PYXIS_DIR).join(SPILL_DIR)
    }

    /// A thread directory nothing wrote through the store: `bytes` are SPARSE,
    /// so a directory can weigh hundreds of mebibytes without costing the disk
    /// anything, and the cap under test stays the real constant.
    fn fake_thread(parent: &Path, name: &str, bytes: u64, seconds_ago: i64) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("artifact.txt"))
            .unwrap()
            .set_len(bytes)
            .unwrap();
        // Last, because creating the file inside bumps the timestamp again.
        age(&dir, seconds_ago);
        dir
    }

    /// Dates a directory explicitly: two directories created in the same
    /// millisecond would otherwise carry the same timestamp, and "oldest first"
    /// would be whatever the filesystem happened to return.
    fn age(path: &Path, seconds_ago: i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let when = nix::sys::time::TimeVal::new(now - seconds_ago, 0);
        nix::sys::stat::utimes(path, &when, &when).unwrap();
    }

    #[test]
    fn a_root_under_the_cap_evicts_nothing_when_a_thread_starts() {
        let ws = Workspace::new();
        let parent = spill_parent(&ws);
        std::fs::create_dir_all(&parent).unwrap();
        let old = fake_thread(&parent, "aaaaaaaaaaaa", MAX_SPILL_ROOT_BYTES / 2, 10_000);
        let recent = fake_thread(&parent, "bbbbbbbbbbbb", 1024, 10);

        let store = SpillStore::create(ws.path(), "thread_1").unwrap();

        assert!(old.is_dir(), "nothing is evicted below the cap");
        assert!(recent.is_dir());
        assert!(store.root().is_dir());
    }

    /// US-081 AC2, from the entry point the binary really calls: creating the
    /// store IS the start of a thread.
    #[test]
    fn a_root_over_the_cap_loses_its_oldest_thread_directory_first() {
        let ws = Workspace::new();
        let parent = spill_parent(&ws);
        std::fs::create_dir_all(&parent).unwrap();
        let oldest = fake_thread(&parent, "aaaaaaaaaaaa", MAX_SPILL_ROOT_BYTES, 10_000);
        let newer = fake_thread(&parent, "bbbbbbbbbbbb", 1024, 10);

        let store = SpillStore::create(ws.path(), "thread_1").unwrap();

        assert!(!oldest.exists(), "the oldest thread directory is evicted");
        assert!(
            newer.is_dir(),
            "eviction stops as soon as the root is back under the cap"
        );
        assert!(
            store.root().is_dir(),
            "the starting thread has its directory"
        );
    }

    /// US-081 AC2, the exclusion: the directory of the thread starting right now
    /// is never a candidate, even when it is both the oldest and the heaviest.
    #[test]
    fn the_directory_of_the_starting_thread_is_never_evicted() {
        let ws = Workspace::new();
        let parent = spill_parent(&ws);
        std::fs::create_dir_all(&parent).unwrap();
        // Pre-create the very directory `create` is about to adopt, over the cap
        // on its own and older than everything else.
        let mine = fake_thread(
            &parent,
            &thread_dir_name("thread_1"),
            MAX_SPILL_ROOT_BYTES + 1,
            10_000,
        );
        let other = fake_thread(&parent, "bbbbbbbbbbbb", 1024, 10);

        let store = SpillStore::create(ws.path(), "thread_1").unwrap();

        assert_eq!(store.root(), mine);
        assert!(
            mine.join("artifact.txt").is_file(),
            "the current thread keeps its artifacts even over the cap"
        );
        assert!(
            !other.exists(),
            "the sweep frees what it can instead of giving up"
        );
    }

    /// US-081 AC3: the one destructive call of the subsystem refuses anything
    /// that is not a thread directory of the root, before the syscall.
    #[test]
    fn an_eviction_target_outside_the_root_is_refused_before_the_syscall() {
        let ws = Workspace::new();
        let parent = spill_parent(&ws);
        std::fs::create_dir_all(&parent).unwrap();
        let outside = ws.path().join("precious");
        std::fs::create_dir_all(&outside).unwrap();

        for candidate in [
            outside.clone(),
            parent.join("..").join("precious"),
            parent.join("a").join("b"),
            parent.clone(),
        ] {
            let err = remove_under(&parent, &candidate).unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "{candidate:?}"
            );
        }
        assert!(outside.is_dir(), "nothing outside the root is ever removed");
    }

    /// US-081 unhappy path: a directory the sweep cannot delete is reported and
    /// the thread starts anyway.
    #[test]
    fn a_directory_that_cannot_be_removed_does_not_prevent_the_thread_from_starting() {
        let ws = Workspace::new();
        let parent = spill_parent(&ws);
        std::fs::create_dir_all(&parent).unwrap();
        let locked = fake_thread(&parent, "aaaaaaaaaaaa", MAX_SPILL_ROOT_BYTES + 1, 10_000);
        // Readable, so the sweep still sizes it, but its entries cannot be
        // unlinked: `remove_dir_all` fails halfway.
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&locked, perms).unwrap();

        let created = SpillStore::create(ws.path(), "thread_1");

        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(DIR_MODE);
        std::fs::set_permissions(&locked, perms).unwrap();

        let store = created.expect("an eviction failure never fails the start");
        assert!(store.root().is_dir());
        assert!(locked.join("artifact.txt").is_file());
    }
}
