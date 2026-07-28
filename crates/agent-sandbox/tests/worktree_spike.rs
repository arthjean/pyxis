//! Spike US-016 : un sous-agent mutateur peut-il vivre dans un worktree Git
//! isolé, sous le confinement actuel et sans exposer `.git` au modèle ?
//!
//! Aucune surface de production n'est créée ici. Ce fichier est l'instrument de
//! mesure du verdict consigné dans `docs/DECISIONS.md` (ADR-13) ; il répond à
//! trois questions et à elles seules :
//!
//! 1. la mécanique passe-t-elle sous Landlock (création, écriture, nettoyage) ?
//! 2. le confinement du noyau protège-t-il le worktree parent d'un enfant ?
//! 3. quels chemins internes faut-il écrire, et que valent les cas dégradés ?
//!
//! `restrict_self` est irréversible et process-wide : appliqué dans le harness,
//! il confinerait `cargo test` lui-même. Le parent relance donc le binaire de
//! test dans un process ENFANT, comme `writable_roots.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CHILD_MARKER: &str = "PYXIS_WORKTREE_CHILD";
const CHILD_REPO: &str = "PYXIS_WORKTREE_CHILD_REPO";
const CHILD_OUTSIDE: &str = "PYXIS_WORKTREE_CHILD_OUTSIDE";

/// AC1, AC2 et AC5 : la mécanique et la frontière réelle du confinement.
#[test]
fn a_worktree_is_reachable_under_confinement_but_isolates_nothing() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        return;
    }
    let Some(repo) = fixture_repo("spike-confine") else {
        println!("skipped: git indisponible");
        return;
    };
    // Témoin hors des racines accordées : ni le workspace, ni `$TMPDIR`.
    let outside = scratch_in(&target_tmp(), "spike-outside");

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_probes_a_worktree_under_confinement",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .env(CHILD_REPO, &repo)
        .env(CHILD_OUTSIDE, &outside)
        .output()
        .expect("relance du binaire de test");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // AC4 : le spike ne fusionne, ne commite, n'applique et ne supprime rien
    // dans le dépôt source. Vérifié AVANT le nettoyage.
    let log = git(&repo, &["log", "--oneline"]);
    let commits = String::from_utf8_lossy(&log.stdout).lines().count();
    let status = git(&repo, &["status", "--porcelain"]);
    let dirty = String::from_utf8_lossy(&status.stdout).into_owned();

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&outside);

    assert!(
        output.status.success(),
        "l'enfant confiné a échoué.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    for line in stdout
        .lines()
        .filter(|l| l.contains("verdict:") || l.contains("skipped:"))
    {
        println!("{line}");
    }
    assert!(
        stdout.contains("verdict:") || stdout.contains("skipped:"),
        "l'enfant n'a rien prouvé.\n--- stdout ---\n{stdout}"
    );
    assert_eq!(
        commits, 1,
        "le spike n'a produit aucun commit dans la source"
    );
    assert!(
        dirty.trim().is_empty(),
        "le spike a laissé le dépôt source sale : {dirty}"
    );
}

/// Corps exécuté UNIQUEMENT dans le process enfant : il applique un confinement
/// irréversible.
#[test]
#[ignore = "exécuté seulement comme process enfant confiné"]
fn child_probes_a_worktree_under_confinement() {
    let Some(repo) = std::env::var_os(CHILD_REPO).map(PathBuf::from) else {
        return;
    };
    let outside = std::env::var_os(CHILD_OUTSIDE)
        .map(PathBuf::from)
        .expect("témoin fourni par le parent");

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots = agent_sandbox::resolve_writable_roots(&[], home.as_deref());
    let status = agent_sandbox::enforce_process(&repo, &[], &roots.as_paths())
        .expect("application du confinement");
    if matches!(
        status,
        agent_sandbox::SandboxStatus::NotEnforced
            | agent_sandbox::SandboxStatus::UnsupportedPlatform
    ) {
        println!("skipped: {status:?}");
        return;
    }

    let worktree = std::env::temp_dir().join(format!("pyxis-worktree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&worktree);

    // 1. AC1 : création dans le répertoire temporaire, déjà accordé en écriture.
    let added = git(
        &repo,
        &[
            "worktree",
            "add",
            &worktree.to_string_lossy(),
            "-b",
            "spike",
        ],
    );
    assert!(
        added.status.success(),
        "création du worktree refusée sous confinement: {}",
        String::from_utf8_lossy(&added.stderr)
    );

    // 2. AC1 : l'enfant modifie SA copie sans toucher au worktree parent.
    std::fs::write(worktree.join("fichier.txt"), b"version enfant")
        .expect("écriture dans le worktree enfant");
    assert_eq!(
        std::fs::read_to_string(repo.join("fichier.txt")).unwrap(),
        "version parente\n",
        "le worktree parent doit rester intact"
    );

    // 3. AC2 : ce que la création a réellement écrit dans le dépôt source.
    let admin =
        repo.join(".git/worktrees/pyxis-worktree-".to_string() + &std::process::id().to_string());
    assert!(
        admin.is_dir(),
        "git a écrit ses fichiers d'administration dans .git/worktrees: {}",
        admin.display()
    );
    let pointer = std::fs::read_to_string(worktree.join(".git")).expect("pointeur gitdir");
    assert!(
        pointer.starts_with("gitdir:"),
        "pointeur inattendu: {pointer}"
    );

    // 4. AC2/AC5 : le confinement n'isole PAS l'enfant du parent. Landlock est
    //    process-wide et les sous-agents de Pyxis sont des tâches du MÊME
    //    process : le worktree parent reste écrivable depuis l'enfant.
    let leak = std::fs::write(repo.join("preuve-de-fuite.txt"), b"le noyau ne separe rien");
    let parent_writable = leak.is_ok();
    let _ = std::fs::remove_file(repo.join("preuve-de-fuite.txt"));

    // 5. Le confinement n'a pas été élargi pour autant.
    assert!(
        std::fs::write(outside.join("escape.txt"), b"nope").is_err(),
        "écriture hors racines accordées acceptée: le confinement ne tient plus"
    );

    // 6. AC1/AC3 : nettoyage, y compris l'administration dans `.git`.
    let removed = git(
        &repo,
        &["worktree", "remove", "--force", &worktree.to_string_lossy()],
    );
    assert!(
        removed.status.success(),
        "suppression du worktree refusée: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !admin.exists(),
        "l'administration doit disparaître au cleanup"
    );
    let _ = git(&repo, &["branch", "-D", "spike"]);

    println!(
        "verdict: mécanique OK sous {status:?}; \
         écritures internes requises = .git/worktrees/<nom>/ (gitdir, HEAD, commondir) \
         et <worktree>/.git; \
         isolation noyau parent<-enfant = {}",
        if parent_writable {
            "ABSENTE (même process, Landlock process-wide)"
        } else {
            "présente"
        }
    );
    assert!(
        parent_writable,
        "si cette assertion tombe, le modèle de confinement a changé et le verdict d'ADR-13 doit être rejoué"
    );
}

/// AC3 : chaque cas dégradé produit un verdict et une procédure de
/// récupération. Aucun confinement ici : ces cas sont ceux de `git`, pas ceux du
/// noyau.
#[test]
fn each_degraded_case_gets_a_verdict_and_a_recovery() {
    let Some(repo) = fixture_repo("spike-degrade") else {
        println!("skipped: git indisponible");
        return;
    };
    let base = target_tmp();

    // 1. Dépôt sale : `git worktree add` n'exige pas un arbre propre.
    std::fs::write(repo.join("fichier.txt"), b"modification non commitee").unwrap();
    let dirty_wt = base.join("spike-wt-dirty");
    let added = git(
        &repo,
        &["worktree", "add", &dirty_wt.to_string_lossy(), "-b", "sale"],
    );
    println!(
        "verdict dépôt sale: {} (récupération: aucune, la branche part du HEAD commité)",
        verdict(&added)
    );
    assert!(
        added.status.success(),
        "un dépôt sale ne bloque pas la création"
    );
    git(&repo, &["checkout", "--", "fichier.txt"]);

    // 2. Worktree verrouillé : la suppression échoue, `unlock` la débloque.
    let locked = git(&repo, &["worktree", "lock", &dirty_wt.to_string_lossy()]);
    assert!(locked.status.success());
    let refused = git(&repo, &["worktree", "remove", &dirty_wt.to_string_lossy()]);
    println!(
        "verdict worktree verrouillé: {} (récupération: `git worktree unlock <chemin>` puis remove)",
        verdict(&refused)
    );
    assert!(
        !refused.status.success(),
        "un worktree verrouillé doit refuser sa suppression"
    );
    git(&repo, &["worktree", "unlock", &dirty_wt.to_string_lossy()]);
    assert!(
        git(
            &repo,
            &["worktree", "remove", "--force", &dirty_wt.to_string_lossy()]
        )
        .status
        .success()
    );
    git(&repo, &["branch", "-D", "sale"]);

    // 3. Cleanup en échec : répertoire effacé sous git, `prune` répare
    //    l'administration restée dans `.git`.
    let orphan = base.join("spike-wt-orphan");
    assert!(
        git(
            &repo,
            &[
                "worktree",
                "add",
                &orphan.to_string_lossy(),
                "-b",
                "orphelin"
            ]
        )
        .status
        .success()
    );
    std::fs::remove_dir_all(&orphan).unwrap();
    let listed = git(&repo, &["worktree", "list"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("spike-wt-orphan"),
        "l'administration survit à la disparition du répertoire"
    );
    let pruned = git(&repo, &["worktree", "prune"]);
    println!(
        "verdict cleanup en échec: {} (récupération: `git worktree prune`)",
        verdict(&pruned)
    );
    assert!(
        !String::from_utf8_lossy(&git(&repo, &["worktree", "list"]).stdout)
            .contains("spike-wt-orphan"),
        "prune doit nettoyer l'administration orpheline"
    );
    git(&repo, &["branch", "-D", "orphelin"]);

    // 4. Hors de tout dépôt Git : refus net, rien à récupérer. Le plafond de
    //    découverte est posé explicitement, sinon `git` remonte l'arborescence.
    let plain = scratch_in(&std::env::temp_dir(), "spike-non-git");
    let ceiling = plain.parent().unwrap().to_path_buf();
    let outside_repo = Command::new("git")
        .current_dir(&plain)
        .args(["worktree", "add", "peu-importe"])
        .env("GIT_CEILING_DIRECTORIES", &ceiling)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git est lançable");
    println!(
        "verdict hors dépôt Git: {} (récupération: refuser l'enfant mutateur)",
        verdict(&outside_repo)
    );
    assert!(
        !outside_repo.status.success(),
        "hors de tout dépôt Git, la création doit échouer"
    );

    // 5. DANGER mesuré : un répertoire non-Git IMBRIQUÉ dans un dépôt n'est pas
    //    un cas d'échec. `git` remonte jusqu'au dépôt englobant et y créerait
    //    silencieusement un worktree. Vérifié en lecture seule : le spike ne
    //    crée rien dans le dépôt qui l'héberge.
    let nested = scratch_in(&base, "spike-nested");
    let discovered = git(&nested, &["rev-parse", "--show-toplevel"]);
    if discovered.status.success() {
        println!(
            "verdict répertoire non-Git imbriqué: REMONTE vers {} \
             (récupération: ancrer l'enfant sur une racine de dépôt explicite, jamais sur un cwd)",
            String::from_utf8_lossy(&discovered.stdout).trim()
        );
    }
    let _ = std::fs::remove_dir_all(&nested);

    // AC4 : rien n'a été commité, fusionné ni appliqué dans la source.
    let commits = String::from_utf8_lossy(&git(&repo, &["log", "--oneline"]).stdout)
        .lines()
        .count();
    assert_eq!(commits, 1, "le spike ne commite jamais");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&plain);
}

fn verdict(output: &Output) -> &'static str {
    if output.status.success() {
        "PASSE"
    } else {
        "REFUS"
    }
}

/// `git` sans configuration utilisateur : le spike ne doit dépendre ni du
/// `~/.gitconfig` d'Arthur ni de celui de la CI.
fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "pyxis-spike")
        .env("GIT_AUTHOR_EMAIL", "spike@pyxis.invalid")
        .env("GIT_COMMITTER_NAME", "pyxis-spike")
        .env("GIT_COMMITTER_EMAIL", "spike@pyxis.invalid")
        .output()
        .expect("git est lançable")
}

/// Dépôt de fixture à un commit. `None` quand `git` est absent.
fn fixture_repo(tag: &str) -> Option<PathBuf> {
    if Command::new("git").arg("--version").output().is_err() {
        return None;
    }
    let repo = scratch_in(&target_tmp(), tag);
    if !git(&repo, &["init", "-b", "main"]).status.success() {
        return None;
    }
    std::fs::write(repo.join("fichier.txt"), b"version parente\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    Some(repo)
}

fn scratch_in(base: &Path, tag: &str) -> PathBuf {
    let dir = base.join(format!("pyxis-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// `target/` du workspace : écrivable, hors `$TMPDIR`, ignoré par git.
fn target_tmp() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}
