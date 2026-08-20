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
        let root = workspace.join(PYXIS_DIR).join(SPILL_DIR).join(&name);
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&root)
            .map_err(|source| SpillError::Directory {
                path: root.display().to_string(),
                source,
            })?;
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
        let file_name = self.file_name(tool_name, call_id);
        let path = self.root.join(&file_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
            .map_err(|source| SpillError::File {
                path: path.display().to_string(),
                source,
            })?;
        file.write_all(content.as_bytes())
            .map_err(|source| SpillError::File {
                path: path.display().to_string(),
                source,
            })?;
        Ok(SpillRef {
            locator: format!("{}/{}", self.locator_prefix, file_name),
            bytes: content.len(),
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
}
