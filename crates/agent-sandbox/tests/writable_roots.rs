//! Preuve du confinement réel des racines writables (US-012 AC1/AC5).
//!
//! `restrict_self` est irréversible et s'applique au process entier : appliqué
//! dans le harness, il confinerait `cargo test` lui-même. Le test parent relance
//! donc le binaire de test dans un process ENFANT, qui pose le confinement puis
//! tente trois écritures : sous le workspace, sous le répertoire temporaire, et
//! hors des deux. L'enfant échoue par `exit(1)` avec un message explicite ; le
//! parent en fait sa propre assertion.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Marque le process enfant et lui transmet les chemins à sonder.
const CHILD_MARKER: &str = "PYXIS_SANDBOX_CHILD";
const CHILD_WORKSPACE: &str = "PYXIS_SANDBOX_CHILD_WORKSPACE";
const CHILD_OUTSIDE: &str = "PYXIS_SANDBOX_CHILD_OUTSIDE";

#[test]
fn temp_dir_is_writable_under_confinement_and_the_rest_is_not() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        return;
    }
    let workspace = scratch("ws");
    // Témoin hors racines accordées : un répertoire writable par l'utilisateur qui
    // n'est ni le workspace ni le répertoire temporaire. `target/` convient : il
    // est ignoré par git et vit hors de `$TMPDIR`.
    let outside = scratch_in(&repo_target_dir(), "outside");

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_applies_confinement_then_probes_writes",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .env(CHILD_WORKSPACE, &workspace)
        .env(CHILD_OUTSIDE, &outside)
        .output()
        .expect("relance du binaire de test");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&outside);

    assert!(
        output.status.success(),
        "l'enfant confiné a échoué.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    // Un enfant vert mais muet signifierait un test creux : il doit dire s'il a
    // prouvé le confinement ou s'il l'a sauté faute de support kernel. La ligne
    // est reprise ici pour être lisible sous `--nocapture`.
    // `--nocapture` colle la sortie du test à la ligne d'entête de libtest : on
    // cherche donc le marqueur DANS la ligne, pas en tête de ligne.
    if let Some(verdict) = stdout
        .lines()
        .find(|l| l.contains("confinement proven") || l.contains("skipped:"))
    {
        println!("child verdict: {verdict}");
    }
    assert!(
        stdout.contains("confinement proven") || stdout.contains("skipped:"),
        "l'enfant n'a rien prouvé.\n--- stdout ---\n{stdout}"
    );
}

/// Corps exécuté UNIQUEMENT dans le process enfant : il pose un confinement
/// irréversible, donc il ne doit jamais tourner dans le harness principal.
#[test]
#[ignore = "exécuté seulement comme process enfant confiné"]
fn child_applies_confinement_then_probes_writes() {
    let Some(workspace) = std::env::var_os(CHILD_WORKSPACE).map(PathBuf::from) else {
        return;
    };
    let outside = std::env::var_os(CHILD_OUTSIDE)
        .map(PathBuf::from)
        .expect("chemin témoin fourni par le parent");

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots = agent_sandbox::resolve_writable_roots(&[], home.as_deref());
    let temp_root = std::fs::canonicalize(std::env::temp_dir()).expect("répertoire temporaire");
    assert!(
        roots.granted.contains(&temp_root),
        "le répertoire temporaire doit être accordé par défaut: {roots:?}"
    );

    let status = agent_sandbox::enforce_process(&workspace, &[], &roots.as_paths())
        .expect("application du confinement");
    if status == agent_sandbox::SandboxStatus::NotEnforced
        || status == agent_sandbox::SandboxStatus::UnsupportedPlatform
    {
        // Kernel sans Landlock effectif : rien à prouver ici, le harness ne peut
        // pas fabriquer une garantie que le noyau ne rend pas.
        println!("skipped: {status:?}");
        return;
    }

    // AC1 : le répertoire temporaire reste writable, y compris pour la création
    // d'un sous-répertoire (`mktemp -d`).
    let temp_dir = temp_root.join(format!("pyxis-sandbox-probe-{}", std::process::id()));
    assert_write_succeeds(&temp_dir, "répertoire temporaire");

    // Le workspace reste writable (comportement d'avant cette story).
    assert_write_succeeds(&workspace.join("nested"), "workspace");

    // Hors racines accordées, l'écriture doit être refusée par le kernel.
    let refused = std::fs::write(outside.join("escape.txt"), b"nope");
    assert!(
        refused.is_err(),
        "écriture hors racines accordées acceptée : le confinement ne tient pas ({})",
        outside.display()
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
    println!("confinement proven: {status:?}");
}

fn assert_write_succeeds(dir: &Path, label: &str) {
    std::fs::create_dir_all(dir)
        .unwrap_or_else(|e| panic!("{label}: création de {} refusée: {e}", dir.display()));
    let file = dir.join("probe.txt");
    std::fs::write(&file, b"ok")
        .unwrap_or_else(|e| panic!("{label}: écriture de {} refusée: {e}", file.display()));
}

fn scratch(tag: &str) -> PathBuf {
    scratch_in(&std::env::temp_dir(), tag)
}

fn scratch_in(base: &Path, tag: &str) -> PathBuf {
    let dir = base.join(format!("pyxis-sandbox-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// `target/` du workspace cargo : writable, hors `$TMPDIR`, ignoré par git.
fn repo_target_dir() -> PathBuf {
    // `CARGO_TARGET_TMPDIR` pointe sous `target/`, et cargo le crée pour les tests
    // d'intégration. C'est exactement le témoin recherché.
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}
