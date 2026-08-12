//! Aggregated diff of a turn.
//!
//! The question this module answers: "what did this turn change on disk?",
//! including what no editing tool produced, for instance a `cargo fmt` or a
//! `sed -i` launched by `bash`. It is the only gap in the audit where Pyxis can
//! be ahead rather than catching up, so the data is structured
//! (`agent_core::TurnDiffView`) and not merely displayed.
//!
//! **Scope: git.** The PRD left the question open; the chosen answer is to
//! delegate discovery to `git status`, which rules out ignored files
//! (`target/`, `node_modules/`) without which fingerprinting the whole
//! workspace would cost several seconds per turn, and without adding a
//! filesystem-watching dependency. Accepted consequence: in a directory that is
//! not a git repository, the aggregated diff is always empty.
//!
//! **How the comparison works.** When the turn opens, the content of every file
//! git reports as dirty is memorized. At close time, the same list is recomputed.
//! A file that appears in the second list was clean at the start, so its
//! starting state is exactly its content at `HEAD`; a file that disappeared
//! from the list was brought back to `HEAD`. A file absent from both lists
//! has no net change, which handles the "created then deleted in the same
//! turn" case for free.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_core::{FileChange, FileDiffView, TurnDiffView};

/// Past this size, a file is listed as modified without being compared line by
/// line: a multi-megabyte diff informs nobody and would make the end of the
/// turn expensive.
const MAX_DIFF_BYTES: usize = 512 * 1024;

/// Execution bound for a git command. A `git status` that never returns
/// (lock, network filesystem) must not suspend the end of the turn.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Context lines of the unified diff, like `git diff`.
const CONTEXT_RADIUS: usize = 3;

/// Internal Pyxis state, never user content: the session file is rewritten on
/// every turn, so including it would show a modification in absolutely every
/// diff, and would copy the transcript into it. Excluded here and not only
/// through `.gitignore`, whose content we do not control.
const EXCLUDED_PREFIX: &str = ".pyxis/";

/// State of a file at a given instant.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    /// The file does not exist.
    Absent,
    /// UTF-8 content, comparable line by line.
    Text(String),
    /// Binary or too large: only its identity is tracked. The hash has no
    /// security role, only change detection within the current
    /// process.
    Opaque(u64),
}

impl FileState {
    fn exists(&self) -> bool {
        !matches!(self, FileState::Absent)
    }
}

/// Reference taken when a turn opens, then compared at its close.
pub struct TurnDiffTracker {
    /// Repository root: the paths reported by `git status --porcelain` are
    /// relative to it, and `git show HEAD:<path>` expects that form.
    repo_root: PathBuf,
    /// Workspace root, to make displayed paths relative to what the user
    /// sees.
    workspace: PathBuf,
    /// `None` when there is no git repository: the tracker goes inert. This
    /// is not an error, it is an absence of scope.
    baseline: Option<BTreeMap<String, FileState>>,
}

/// Result of an on-demand workspace diff (US-006). The absence of a git
/// repository is not an error: it is an absence of scope, and the caller must be
/// able to say so instead of showing an empty diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceDiff {
    NoRepository,
    Changes(TurnDiffView),
}

/// Current modifications of the workspace against `HEAD` (US-006). Reuses the
/// turn engine with an EMPTY baseline: "nothing was dirty when we started"
/// means every file git reports today is compared to its `HEAD` state, which is
/// exactly the working-tree diff, with the same scope, the same exclusions and
/// the same size limits as the aggregated turn diff.
pub async fn workspace_diff(workspace: &Path) -> Result<WorkspaceDiff, String> {
    let Some(repo_root) = repo_root(workspace).await else {
        return Ok(WorkspaceDiff::NoRepository);
    };
    let mut tracker = TurnDiffTracker {
        repo_root,
        workspace: workspace.to_path_buf(),
        baseline: Some(BTreeMap::new()),
    };
    tracker.turn_diff().await.map(WorkspaceDiff::Changes)
}

impl TurnDiffTracker {
    /// Captures the reference. Never returns an error: a turn must not fail
    /// because its observability failed.
    pub async fn begin(workspace: &Path) -> Self {
        let mut tracker = Self {
            repo_root: workspace.to_path_buf(),
            workspace: workspace.to_path_buf(),
            baseline: None,
        };
        let Some(repo_root) = repo_root(workspace).await else {
            return tracker;
        };
        tracker.repo_root = repo_root;
        match tracker.snapshot_dirty().await {
            Ok(baseline) => tracker.baseline = Some(baseline),
            // The repository exists but git failed: the turn goes on without a diff.
            // Reported through the facade (US-020): a library never writes on a
            // process output, the TUI owns that terminal.
            Err(err) => {
                tracing::warn!(target: "pyxis::diff", %err, "turn diff baseline unavailable")
            }
        }
        tracker
    }

    /// Aggregated diff of the turn. Empty when nothing changed, or when there is
    /// no git scope to observe.
    pub async fn turn_diff(&mut self) -> Result<TurnDiffView, String> {
        let Some(baseline) = self.baseline.as_ref() else {
            return Ok(TurnDiffView::default());
        };
        let after = self.snapshot_dirty().await?;

        let mut changed: Vec<(String, FileState, FileState)> = Vec::new();
        let mut paths: Vec<&String> = baseline.keys().chain(after.keys()).collect();
        paths.sort_unstable();
        paths.dedup();

        for path in paths {
            // A path absent from one of the two lists was clean at that
            // moment, so identical to `HEAD`.
            let before = match baseline.get(path) {
                Some(state) => state.clone(),
                None => self.head_state(path).await,
            };
            let after_state = match after.get(path) {
                Some(state) => state.clone(),
                None => self.head_state(path).await,
            };
            if before == after_state {
                continue;
            }
            changed.push((path.clone(), before, after_state));
        }

        // Renames are paired BEFORE the diffs are built: the pairing needs the
        // content on both sides, which the rendered view no longer carries.
        let renamed_from = pair_renames(&changed);
        let mut files = Vec::with_capacity(changed.len());
        for (path, before, after_state) in &changed {
            if renamed_from.values().any(|source| source == path) {
                // The source of a rename: it is reported on the destination
                // entry, not as a deletion of its own.
                continue;
            }
            let mut view = self.file_diff(path, before, after_state);
            if let Some(source) = renamed_from.get(path) {
                view.change = FileChange::Renamed;
                view.moved_from = Some(self.display_path(source));
                // A pure rename moves no line: the counts would otherwise
                // report the whole file as added.
                view.added_lines = 0;
                view.removed_lines = 0;
                view.unified = None;
            }
            files.push(view);
        }

        Ok(TurnDiffView { files })
    }

    fn file_diff(&self, repo_path: &str, before: &FileState, after: &FileState) -> FileDiffView {
        let change = match (before.exists(), after.exists()) {
            (false, true) => FileChange::Added,
            (true, false) => FileChange::Deleted,
            _ => FileChange::Modified,
        };
        let display = self.display_path(repo_path);
        match (before, after) {
            (FileState::Text(before), FileState::Text(after)) => {
                let (added, removed, unified) = unified_diff(&display, before, after);
                FileDiffView {
                    path: display,
                    change,
                    added_lines: added,
                    removed_lines: removed,
                    unified: Some(unified),
                    moved_from: None,
                }
            }
            (FileState::Absent, FileState::Text(after)) => {
                let (added, removed, unified) = unified_diff(&display, "", after);
                FileDiffView {
                    path: display,
                    change,
                    added_lines: added,
                    removed_lines: removed,
                    unified: Some(unified),
                    moved_from: None,
                }
            }
            (FileState::Text(before), FileState::Absent) => {
                let (added, removed, unified) = unified_diff(&display, before, "");
                FileDiffView {
                    path: display,
                    change,
                    added_lines: added,
                    removed_lines: removed,
                    unified: Some(unified),
                    moved_from: None,
                }
            }
            // At least one side is binary or too large: listed without a diff
            // (AC4), and without a line count that would be wrong.
            _ => FileDiffView {
                path: display,
                change,
                added_lines: 0,
                removed_lines: 0,
                unified: None,
                moved_from: None,
            },
        }
    }

    /// Path as the user refers to it: relative to the workspace when the
    /// workspace is a subdirectory of the repository, relative to the repository
    /// otherwise.
    fn display_path(&self, repo_path: &str) -> String {
        let absolute = self.repo_root.join(repo_path);
        absolute
            .strip_prefix(&self.workspace)
            .map(|rel| rel.to_string_lossy().to_string())
            .unwrap_or_else(|_| repo_path.to_string())
    }

    /// Content and state of every file git reports as different from `HEAD`,
    /// untracked files included, ignored files excluded. The `.` pathspec
    /// bounds the list to the workspace when it is a subdirectory of the repository.
    async fn snapshot_dirty(&self) -> Result<BTreeMap<String, FileState>, String> {
        let out = git(
            &self.workspace,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-uall",
                "-z",
                "--no-renames",
                "--",
                ".",
            ],
        )
        .await?;

        let mut snapshot = BTreeMap::new();
        // `-z` format: `XY <path>\0`, without filename escaping.
        // `--no-renames` guarantees that no entry carries a second path.
        for entry in out.split('\0').filter(|entry| entry.len() > 3) {
            let path = &entry[3..];
            if self.is_excluded(path) {
                continue;
            }
            let state = self.read_worktree(path);
            snapshot.insert(path.to_string(), state);
        }
        Ok(snapshot)
    }

    /// The exclusion applies to the path **as seen by the user**: the workspace
    /// `.pyxis/`, not a same-named `.pyxis/` located elsewhere in the repository.
    fn is_excluded(&self, repo_path: &str) -> bool {
        self.display_path(repo_path).starts_with(EXCLUDED_PREFIX)
    }

    fn read_worktree(&self, repo_path: &str) -> FileState {
        match std::fs::read(self.repo_root.join(repo_path)) {
            Ok(bytes) => state_from_bytes(&bytes),
            Err(_) => FileState::Absent,
        }
    }

    /// State of the file at `HEAD`. Absent when the path is not there (file created
    /// during the turn, or repository still without a commit).
    async fn head_state(&self, repo_path: &str) -> FileState {
        let spec = format!("HEAD:{repo_path}");
        match git_bytes(&self.repo_root, &["--no-optional-locks", "show", &spec]).await {
            Ok(bytes) => state_from_bytes(&bytes),
            Err(_) => FileState::Absent,
        }
    }
}

/// Pairs each disappeared path with a reappeared one holding the SAME content:
/// destination -> source.
///
/// Only exact content matches pair up. Git decides renames with a similarity
/// threshold; applying one here would mean guessing, and a wrong guess reports
/// two unrelated files as one move. A content that appears twice is left alone
/// for the same reason: nothing designates which copy is "the" rename.
fn pair_renames(changed: &[(String, FileState, FileState)]) -> BTreeMap<String, String> {
    // An empty file never pairs: every empty file has the same content, so the
    // match would carry no evidence of an actual move.
    let pairable = |state: &FileState| match state {
        FileState::Absent => false,
        FileState::Text(text) => !text.is_empty(),
        FileState::Opaque(_) => true,
    };
    let disappeared: Vec<(&String, &FileState)> = changed
        .iter()
        .filter(|(_, before, after)| !after.exists() && pairable(before))
        .map(|(path, before, _)| (path, before))
        .collect();

    let mut renamed_from = BTreeMap::new();
    let mut claimed: Vec<&String> = Vec::new();
    for (path, before, after) in changed {
        if before.exists() || !pairable(after) {
            continue;
        }
        let available: Vec<&String> = disappeared
            .iter()
            .filter(|(source, state)| *state == after && !claimed.contains(source))
            .map(|(source, _)| *source)
            .collect();
        // Exactly one unclaimed source, otherwise the move is ambiguous.
        if let [source] = available.as_slice() {
            claimed.push(source);
            renamed_from.insert(path.clone(), (*source).clone());
        }
    }
    renamed_from
}

fn state_from_bytes(bytes: &[u8]) -> FileState {
    if bytes.len() > MAX_DIFF_BYTES || bytes.contains(&0) {
        return FileState::Opaque(hash_bytes(bytes));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => FileState::Text(text.to_string()),
        Err(_) => FileState::Opaque(hash_bytes(bytes)),
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn unified_diff(path: &str, before: &str, after: &str) -> (u32, u32, String) {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0u32;
    let mut removed = 0u32;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added = added.saturating_add(1),
            similar::ChangeTag::Delete => removed = removed.saturating_add(1),
            similar::ChangeTag::Equal => {}
        }
    }
    let unified = diff
        .unified_diff()
        .context_radius(CONTEXT_RADIUS)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    (added, removed, unified)
}

/// Root of the repository containing `dir`, or `None` if `dir` is not in a repository.
async fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(
        dir,
        &["--no-optional-locks", "rev-parse", "--show-toplevel"],
    )
    .await
    .ok()?;
    let root = out.trim();
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(root))
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = git_bytes(dir, args).await?;
    String::from_utf8(bytes).map_err(|_| "git returned non-utf8 output".to_string())
}

async fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("git {}: timed out", args.join(" ")))?
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {}: {stderr}", args.join(" ")));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway git repository. `-c` rather than a global configuration: the test
    /// must assume nothing about the machine running it.
    struct Repo {
        dir: PathBuf,
    }

    impl Repo {
        async fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("pyxis-turndiff-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // `$TMPDIR` is a symbolic link on some distributions, and
            // `rev-parse --show-toplevel` returns the canonical path.
            let dir = std::fs::canonicalize(&dir).unwrap();
            let repo = Self { dir };
            repo.git(&["init", "-q", "-b", "main"]).await;
            repo
        }

        async fn git(&self, args: &[&str]) {
            let out = tokio::process::Command::new("git")
                .current_dir(&self.dir)
                .args(args)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        async fn commit(&self, message: &str) {
            self.git(&["add", "-A"]).await;
            self.git(&[
                "-c",
                "user.name=pyxis",
                "-c",
                "user.email=pyxis@example.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ])
            .await;
        }

        fn write(&self, name: &str, contents: &str) {
            let path = self.dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }

        fn write_bytes(&self, name: &str, contents: &[u8]) {
            std::fs::write(self.dir.join(name), contents).unwrap();
        }

        fn remove(&self, name: &str) {
            std::fs::remove_file(self.dir.join(name)).unwrap();
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn find<'a>(view: &'a TurnDiffView, path: &str) -> &'a FileDiffView {
        let found = view.files.iter().find(|f| f.path == path);
        assert!(found.is_some(), "{path} absent de {:?}", view.files);
        found.unwrap()
    }

    fn changes(result: WorkspaceDiff) -> Option<TurnDiffView> {
        match result {
            WorkspaceDiff::Changes(view) => Some(view),
            WorkspaceDiff::NoRepository => None,
        }
    }

    /// US-006 AC1: the on-demand diff reports the modifications present at the
    /// moment it is asked for, against `HEAD`, without any turn having opened.
    #[tokio::test]
    async fn workspace_diff_reports_current_changes_against_head() {
        let repo = Repo::new("workspace").await;
        repo.write("kept.txt", "un\ndeux\n");
        repo.commit("initial").await;
        repo.write("kept.txt", "un\ndeux modifie\n");
        repo.write("new.txt", "neuf\n");

        let diff = changes(workspace_diff(&repo.dir).await.unwrap()).expect("dépôt git présent");
        assert_eq!(diff.files.len(), 2, "{:?}", diff.files);
        assert_eq!(find(&diff, "kept.txt").change, FileChange::Modified);
        assert_eq!(find(&diff, "new.txt").change, FileChange::Added);

        // A clean workspace has nothing to report, and that is not an error.
        repo.commit("tout").await;
        let clean = changes(workspace_diff(&repo.dir).await.unwrap()).expect("dépôt git présent");
        assert!(clean.is_empty());
    }

    /// A pure rename is ONE change naming both ends, not a deletion plus an
    /// unrelated creation.
    #[tokio::test]
    async fn a_moved_file_is_reported_as_a_rename() {
        let repo = Repo::new("rename").await;
        repo.write("old.txt", "contenu stable\n");
        repo.commit("initial").await;
        repo.write("new.txt", "contenu stable\n");
        repo.remove("old.txt");

        let diff = changes(workspace_diff(&repo.dir).await.unwrap()).expect("dépôt git présent");
        assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
        let moved = find(&diff, "new.txt");
        assert_eq!(moved.change, FileChange::Renamed);
        assert_eq!(moved.moved_from.as_deref(), Some("old.txt"));
        // A move touches no line: reporting the file as fully added and fully
        // removed would double-count the whole turn.
        assert_eq!((moved.added_lines, moved.removed_lines), (0, 0));
    }

    /// Two files with the same content leave the move ambiguous, so nothing is
    /// paired: a wrong pairing reports two unrelated files as one.
    #[tokio::test]
    async fn an_ambiguous_move_stays_a_delete_and_an_add() {
        let repo = Repo::new("rename-ambiguous").await;
        repo.write("a.txt", "meme contenu\n");
        repo.write("b.txt", "meme contenu\n");
        repo.commit("initial").await;
        repo.write("c.txt", "meme contenu\n");
        repo.remove("a.txt");
        repo.remove("b.txt");

        let diff = changes(workspace_diff(&repo.dir).await.unwrap()).expect("dépôt git présent");
        assert_eq!(find(&diff, "c.txt").change, FileChange::Added);
        assert_eq!(find(&diff, "a.txt").change, FileChange::Deleted);
        assert_eq!(find(&diff, "b.txt").change, FileChange::Deleted);
    }

    /// US-006 AC2: outside a repository the absence of scope is named, never
    /// confused with "nothing changed".
    #[tokio::test]
    async fn workspace_diff_reports_absence_of_repository() {
        let dir = std::env::temp_dir().join(format!("pyxis-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = workspace_diff(&dir).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result, WorkspaceDiff::NoRepository);
    }

    /// AC1 + AC2: a file edited and a file written by a shell command both
    /// show up, without the tracker knowing which is which.
    #[tokio::test]
    async fn aggregates_tool_edits_and_shell_writes() {
        let repo = Repo::new("aggregate").await;
        repo.write("kept.txt", "un\ndeux\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;

        // Edit of a tracked file, as the `edit` tool would do.
        repo.write("kept.txt", "un\ndeux modifie\ntrois\n");
        // Creation by a shell command, invisible to the tool pipeline.
        tokio::process::Command::new("sh")
            .current_dir(&repo.dir)
            .args(["-c", "printf 'genere\\n' > generated.txt"])
            .status()
            .await
            .unwrap();

        let diff = tracker.turn_diff().await.unwrap();

        assert_eq!(diff.files.len(), 2, "{:?}", diff.files);
        let edited = find(&diff, "kept.txt");
        assert_eq!(edited.change, FileChange::Modified);
        assert_eq!((edited.added_lines, edited.removed_lines), (2, 1));
        assert!(edited.unified.as_deref().unwrap().contains("+deux modifie"));

        let generated = find(&diff, "generated.txt");
        assert_eq!(generated.change, FileChange::Added);
        assert_eq!((generated.added_lines, generated.removed_lines), (1, 0));

        assert_eq!(diff.totals(), (3, 1));
    }

    /// AC3: created then deleted in the same turn -> no net change.
    #[tokio::test]
    async fn file_created_then_deleted_is_not_a_net_change() {
        let repo = Repo::new("transient").await;
        repo.write("kept.txt", "stable\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write("scratch.txt", "temporaire\n");
        repo.remove("scratch.txt");

        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty(), "{:?}", diff.files);
    }

    /// AC3, symmetric: a tracked file modified then restored identically does
    /// not count either.
    #[tokio::test]
    async fn tracked_file_restored_to_its_original_content_is_not_a_change() {
        let repo = Repo::new("restored").await;
        repo.write("kept.txt", "origine\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write("kept.txt", "brouillon\n");
        repo.write("kept.txt", "origine\n");

        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty(), "{:?}", diff.files);
    }

    /// AC4: binary and over-threshold file -> listed, never diffed.
    #[tokio::test]
    async fn binary_and_oversized_files_are_listed_without_a_diff() {
        let repo = Repo::new("binary").await;
        repo.write("kept.txt", "x\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write_bytes("image.png", &[0x89, b'P', b'N', b'G', 0x00, 0x1a, 0x0a]);
        repo.write("huge.txt", &"a\n".repeat(MAX_DIFF_BYTES));

        let diff = tracker.turn_diff().await.unwrap();

        let image = find(&diff, "image.png");
        assert_eq!(image.change, FileChange::Added);
        assert!(image.unified.is_none(), "un binaire ne doit pas etre diffe");
        assert_eq!((image.added_lines, image.removed_lines), (0, 0));

        let huge = find(&diff, "huge.txt");
        assert!(huge.unified.is_none(), "au-dela du seuil, pas de diff");
    }

    /// AC5: a turn without a change produces nothing, including when the
    /// workspace was already dirty at the start.
    #[tokio::test]
    async fn a_turn_without_modification_is_empty_even_on_a_dirty_worktree() {
        let repo = Repo::new("dirty").await;
        repo.write("kept.txt", "committe\n");
        repo.commit("initial").await;
        // Pre-existing dirt, not attributable to the turn.
        repo.write("kept.txt", "modifie par l'utilisateur\n");
        repo.write("untracked.txt", "brouillon utilisateur\n");

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty(), "{:?}", diff.files);
    }

    /// Pre-existing dirt does not mask a turn modification on the SAME
    /// file: content, not git status, is what counts.
    #[tokio::test]
    async fn a_file_already_dirty_before_the_turn_is_still_diffed_from_its_turn_start_content() {
        let repo = Repo::new("already-dirty").await;
        repo.write("kept.txt", "v1\n");
        repo.commit("initial").await;
        repo.write("kept.txt", "v2 utilisateur\n");

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write("kept.txt", "v2 utilisateur\nv3 agent\n");

        let diff = tracker.turn_diff().await.unwrap();

        let file = find(&diff, "kept.txt");
        assert_eq!((file.added_lines, file.removed_lines), (1, 0));
        let unified = file.unified.as_deref().unwrap();
        assert!(unified.contains("+v3 agent"), "{unified}");
        assert!(
            !unified.contains("+v2 utilisateur"),
            "la modification de l'utilisateur n'appartient pas au tour: {unified}"
        );
    }

    /// Deletion of a tracked file.
    #[tokio::test]
    async fn deleting_a_tracked_file_is_reported_as_a_deletion() {
        let repo = Repo::new("delete").await;
        repo.write("doomed.txt", "une\ndeux\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.remove("doomed.txt");

        let diff = tracker.turn_diff().await.unwrap();

        let file = find(&diff, "doomed.txt");
        assert_eq!(file.change, FileChange::Deleted);
        assert_eq!((file.added_lines, file.removed_lines), (0, 2));
    }

    /// Ignored files stay out of scope: that is the very reason for
    /// delegating discovery to git.
    #[tokio::test]
    async fn ignored_files_stay_out_of_scope() {
        let repo = Repo::new("ignored").await;
        repo.write(".gitignore", "build/\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write("build/artifact.bin", "sortie de compilation\n");

        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty(), "{:?}", diff.files);
    }

    /// Outside a git repository: inert, without an error (same requirement as
    /// US-013 about a missing `.git`).
    #[tokio::test]
    async fn outside_a_git_repository_the_tracker_is_inert() {
        let dir = std::env::temp_dir().join(format!("pyxis-turndiff-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut tracker = TurnDiffTracker::begin(&dir).await;
        std::fs::write(dir.join("f.txt"), "peu importe\n").unwrap();

        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repository without any commit: `HEAD` does not exist, everything is an addition.
    #[tokio::test]
    async fn a_repository_without_any_commit_reports_additions() {
        let repo = Repo::new("no-head").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write("first.txt", "premier\n");

        let diff = tracker.turn_diff().await.unwrap();

        let file = find(&diff, "first.txt");
        assert_eq!(file.change, FileChange::Added);
        assert_eq!((file.added_lines, file.removed_lines), (1, 0));
    }

    /// Pyxis internal state stays out of the diff, even when the repository does
    /// not ignore it: otherwise every turn would see its own session modified.
    #[tokio::test]
    async fn pyxis_internal_state_is_never_reported() {
        let repo = Repo::new("pyxis-state").await;
        repo.write("kept.txt", "x\n");
        // Deliberately NO .gitignore: the exclusion must not depend on one.
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write(".pyxis/sessions/run.jsonl", "{\"entry\":\"message\"}\n");
        repo.write("kept.txt", "y\n");

        let diff = tracker.turn_diff().await.unwrap();

        assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
        assert_eq!(diff.files[0].path, "kept.txt");
    }

    /// Subdirectory of the repository: the scope and the displayed paths follow the
    /// workspace, not the repository root.
    #[tokio::test]
    async fn a_workspace_subdirectory_scopes_and_relativizes_paths() {
        let repo = Repo::new("subdir").await;
        repo.write("crate-a/src/lib.rs", "// a\n");
        repo.write("crate-b/src/lib.rs", "// b\n");
        repo.commit("initial").await;
        let workspace = repo.dir.join("crate-a");

        let mut tracker = TurnDiffTracker::begin(&workspace).await;
        repo.write("crate-a/src/lib.rs", "// a modifie\n");
        repo.write("crate-b/src/lib.rs", "// b modifie\n");

        let diff = tracker.turn_diff().await.unwrap();

        assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
        assert_eq!(diff.files[0].path, "src/lib.rs");
    }
}
