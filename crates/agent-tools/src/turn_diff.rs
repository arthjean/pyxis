//! Diff agrégé d'un tour (US-018, `tasks/prd-harness-parity.md`).
//!
//! Question à laquelle ce module répond : « qu'est-ce que ce tour a changé sur le
//! disque ? » — y compris ce qu'aucun outil d'édition n'a produit, par exemple un
//! `cargo fmt` ou un `sed -i` lancé par `bash`. C'est le seul écart de l'audit où
//! Pyxis peut être en avance plutôt qu'en rattrapage, donc la donnée est
//! structurée (`agent_core::TurnDiffView`) et pas seulement affichée.
//!
//! **Périmètre : git.** Le PRD laissait la question ouverte ; la réponse retenue
//! est de déléguer la découverte à `git status`, ce qui exclut d'office les
//! fichiers ignorés (`target/`, `node_modules/`) sans lesquels une empreinte du
//! workspace entier coûterait plusieurs secondes par tour, et sans ajouter de
//! dépendance de surveillance du système de fichiers. Conséquence assumée : dans
//! un répertoire qui n'est pas un dépôt git, le diff agrégé est toujours vide.
//!
//! **Comment la comparaison fonctionne.** À l'ouverture du tour, le contenu de
//! chaque fichier que git déclare sale est mémorisé. À la fermeture, la même liste
//! est recalculée. Un fichier apparu dans la seconde liste était propre au départ,
//! donc son état de départ est exactement son contenu à `HEAD` ; un fichier
//! disparu de la liste a été ramené à `HEAD`. Un fichier absent des deux listes
//! n'a pas de modification nette, ce qui traite gratuitement le cas « créé puis
//! supprimé dans le même tour ».

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_core::{FileChange, FileDiffView, TurnDiffView};

/// Au-delà de ce volume, un fichier est listé comme modifié sans être comparé
/// ligne à ligne : un diff de plusieurs mégaoctets n'informe personne et rendrait
/// la fin de tour coûteuse.
const MAX_DIFF_BYTES: usize = 512 * 1024;

/// Borne d'exécution d'une commande git. Un `git status` qui ne rend pas la main
/// (verrou, système de fichiers réseau) ne doit pas suspendre la fin du tour.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Lignes de contexte du diff unifié, comme `git diff`.
const CONTEXT_RADIUS: usize = 3;

/// État interne de Pyxis, jamais du contenu utilisateur : le fichier de session
/// est réécrit à chaque tour, donc l'inclure ferait apparaître une modification
/// dans absolument tous les diffs, et y recopierait le transcript. Exclu ici et
/// pas seulement via `.gitignore`, dont on ne contrôle pas le contenu.
const EXCLUDED_PREFIX: &str = ".pyxis/";

/// État d'un fichier à un instant donné.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    /// Le fichier n'existe pas.
    Absent,
    /// Contenu UTF-8, comparable ligne à ligne.
    Text(String),
    /// Binaire ou trop volumineux : seule son identité est suivie. Le hash n'a
    /// aucun rôle de sécurité, uniquement de détection de changement dans le
    /// processus courant.
    Opaque(u64),
}

impl FileState {
    fn exists(&self) -> bool {
        !matches!(self, FileState::Absent)
    }
}

/// Référence prise à l'ouverture d'un tour, puis comparée à sa fermeture.
pub struct TurnDiffTracker {
    /// Racine du dépôt : les chemins rapportés par `git status --porcelain` lui
    /// sont relatifs, et `git show HEAD:<path>` les attend sous cette forme.
    repo_root: PathBuf,
    /// Racine du workspace, pour rendre les chemins affichés relatifs à ce que
    /// l'utilisateur voit.
    workspace: PathBuf,
    /// `None` quand il n'y a pas de dépôt git : le tracker devient inerte. Ce
    /// n'est pas une erreur, c'est une absence de périmètre.
    baseline: Option<BTreeMap<String, FileState>>,
}

impl TurnDiffTracker {
    /// Capture la référence. Ne renvoie jamais d'erreur : un tour ne doit pas
    /// échouer parce que son observabilité a échoué.
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
            // Le dépôt existe mais git a échoué : le tour continue sans diff.
            Err(err) => eprintln!("[diff] baseline unavailable: {err}"),
        }
        tracker
    }

    /// Diff agrégé du tour. Vide quand rien n'a changé, ou quand il n'y a pas de
    /// périmètre git à observer.
    pub async fn turn_diff(&mut self) -> Result<TurnDiffView, String> {
        let Some(baseline) = self.baseline.as_ref() else {
            return Ok(TurnDiffView::default());
        };
        let after = self.snapshot_dirty().await?;

        let mut files = Vec::new();
        let mut paths: Vec<&String> = baseline.keys().chain(after.keys()).collect();
        paths.sort_unstable();
        paths.dedup();

        for path in paths {
            // Un chemin absent d'une des deux listes était propre à ce
            // moment-là, donc identique à `HEAD`.
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
            files.push(self.file_diff(path, &before, &after_state));
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
                }
            }
            // Au moins un côté est binaire ou trop volumineux : listé sans diff
            // (AC4), et sans compte de lignes qui serait faux.
            _ => FileDiffView {
                path: display,
                change,
                added_lines: 0,
                removed_lines: 0,
                unified: None,
            },
        }
    }

    /// Chemin tel que l'utilisateur le désigne : relatif au workspace quand
    /// celui-ci est un sous-répertoire du dépôt, relatif au dépôt sinon.
    fn display_path(&self, repo_path: &str) -> String {
        let absolute = self.repo_root.join(repo_path);
        absolute
            .strip_prefix(&self.workspace)
            .map(|rel| rel.to_string_lossy().to_string())
            .unwrap_or_else(|_| repo_path.to_string())
    }

    /// Contenu et état de chaque fichier que git déclare différent de `HEAD`,
    /// fichiers non suivis compris, fichiers ignorés exclus. Le pathspec `.`
    /// borne la liste au workspace quand il est un sous-répertoire du dépôt.
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
        // Format `-z` : `XY <path>\0`, sans échappement de nom de fichier.
        // `--no-renames` garantit qu'aucune entrée ne porte un second chemin.
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

    /// L'exclusion porte sur le chemin **vu par l'utilisateur** : `.pyxis/` du
    /// workspace, pas d'un `.pyxis/` homonyme situé ailleurs dans le dépôt.
    fn is_excluded(&self, repo_path: &str) -> bool {
        self.display_path(repo_path).starts_with(EXCLUDED_PREFIX)
    }

    fn read_worktree(&self, repo_path: &str) -> FileState {
        match std::fs::read(self.repo_root.join(repo_path)) {
            Ok(bytes) => state_from_bytes(&bytes),
            Err(_) => FileState::Absent,
        }
    }

    /// État du fichier à `HEAD`. Absent quand le chemin n'y est pas (fichier créé
    /// pendant le tour, ou dépôt encore sans commit).
    async fn head_state(&self, repo_path: &str) -> FileState {
        let spec = format!("HEAD:{repo_path}");
        match git_bytes(&self.repo_root, &["--no-optional-locks", "show", &spec]).await {
            Ok(bytes) => state_from_bytes(&bytes),
            Err(_) => FileState::Absent,
        }
    }
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

/// Racine du dépôt contenant `dir`, ou `None` si `dir` n'est pas dans un dépôt.
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

    /// Dépôt git jetable. `-c` plutôt qu'une configuration globale : le test ne
    /// doit rien supposer de la machine qui l'exécute.
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
            // `$TMPDIR` est un lien symbolique sur certaines distributions, et
            // `rev-parse --show-toplevel` rend le chemin canonique.
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

    /// AC1 + AC2 : un fichier édité et un fichier écrit par une commande shell
    /// apparaissent tous les deux, sans que le tracker sache lequel est lequel.
    #[tokio::test]
    async fn aggregates_tool_edits_and_shell_writes() {
        let repo = Repo::new("aggregate").await;
        repo.write("kept.txt", "un\ndeux\n");
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;

        // Édition d'un fichier suivi, comme le ferait l'outil `edit`.
        repo.write("kept.txt", "un\ndeux modifie\ntrois\n");
        // Création par une commande shell, invisible du pipeline d'outils.
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

    /// AC3 : créé puis supprimé dans le même tour → aucune modification nette.
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

    /// AC3, symétrique : un fichier suivi modifié puis remis à l'identique ne
    /// compte pas non plus.
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

    /// AC4 : binaire et fichier hors seuil → listés, jamais diffés.
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

    /// AC5 : un tour sans modification ne produit rien, y compris quand le
    /// workspace était déjà sale au départ.
    #[tokio::test]
    async fn a_turn_without_modification_is_empty_even_on_a_dirty_worktree() {
        let repo = Repo::new("dirty").await;
        repo.write("kept.txt", "committe\n");
        repo.commit("initial").await;
        // Saleté préexistante, non imputable au tour.
        repo.write("kept.txt", "modifie par l'utilisateur\n");
        repo.write("untracked.txt", "brouillon utilisateur\n");

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        let diff = tracker.turn_diff().await.unwrap();

        assert!(diff.is_empty(), "{:?}", diff.files);
    }

    /// La saleté préexistante ne masque pas une modification du tour sur le MÊME
    /// fichier : c'est le contenu, pas le statut git, qui fait foi.
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

    /// Suppression d'un fichier suivi.
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

    /// Les fichiers ignorés restent hors périmètre : c'est la raison même de
    /// déléguer la découverte à git.
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

    /// Hors dépôt git : inerte, sans erreur (même exigence qu'US-013 sur
    /// l'absence de `.git`).
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

    /// Un dépôt sans aucun commit : `HEAD` n'existe pas, tout est un ajout.
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

    /// L'état interne de Pyxis reste hors du diff, même quand le dépôt ne
    /// l'ignore pas : sinon chaque tour se verrait modifier sa propre session.
    #[tokio::test]
    async fn pyxis_internal_state_is_never_reported() {
        let repo = Repo::new("pyxis-state").await;
        repo.write("kept.txt", "x\n");
        // Volontairement PAS de .gitignore : l'exclusion ne doit pas en dépendre.
        repo.commit("initial").await;

        let mut tracker = TurnDiffTracker::begin(&repo.dir).await;
        repo.write(".pyxis/sessions/run.jsonl", "{\"entry\":\"message\"}\n");
        repo.write("kept.txt", "y\n");

        let diff = tracker.turn_diff().await.unwrap();

        assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
        assert_eq!(diff.files[0].path, "kept.txt");
    }

    /// Sous-répertoire du dépôt : le périmètre et les chemins affichés suivent le
    /// workspace, pas la racine du dépôt.
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
