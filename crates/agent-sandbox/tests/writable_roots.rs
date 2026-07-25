//! Proof of the real confinement of the writable roots (US-012 AC1/AC5).
//!
//! `restrict_self` is irreversible and applies to the whole process: applied
//! in the harness, it would confine `cargo test` itself. The parent test therefore
//! relaunches the test binary in a CHILD process, which applies the confinement then
//! attempts three writes: under the workspace, under the temporary directory, and
//! outside both. The child fails with `exit(1)` and an explicit message; the
//! parent turns it into its own assertion.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Marks the child process and passes it the paths to probe.
const CHILD_MARKER: &str = "PYXIS_SANDBOX_CHILD";
const CHILD_WORKSPACE: &str = "PYXIS_SANDBOX_CHILD_WORKSPACE";
const CHILD_OUTSIDE: &str = "PYXIS_SANDBOX_CHILD_OUTSIDE";

#[test]
fn temp_dir_is_writable_under_confinement_and_the_rest_is_not() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        return;
    }
    let workspace = scratch("ws");
    // Control outside the granted roots: a directory writable by the user that
    // is neither the workspace nor the temporary directory. `target/` fits: it
    // is ignored by git and lives outside `$TMPDIR`.
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
    // A green but silent child would mean a hollow test: it must say whether it
    // proved the confinement or skipped it for lack of kernel support. The line
    // is echoed here to be readable under `--nocapture`.
    // `--nocapture` glues the test output to the libtest header line: so we
    // look for the marker INSIDE the line, not at the start of the line.
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

/// Body executed ONLY in the child process: it applies an irreversible
/// confinement, so it must never run in the main harness.
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
        // Kernel without effective Landlock: nothing to prove here, the harness cannot
        // manufacture a guarantee that the kernel does not provide.
        println!("skipped: {status:?}");
        return;
    }

    // AC1: the temporary directory stays writable, including for creating
    // a subdirectory (`mktemp -d`).
    let temp_dir = temp_root.join(format!("pyxis-sandbox-probe-{}", std::process::id()));
    assert_write_succeeds(&temp_dir, "répertoire temporaire");

    // The workspace stays writable (behavior from before this story).
    assert_write_succeeds(&workspace.join("nested"), "workspace");

    // Outside the granted roots, the write must be refused by the kernel.
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

/// `target/` of the cargo workspace: writable, outside `$TMPDIR`, ignored by git.
fn repo_target_dir() -> PathBuf {
    // `CARGO_TARGET_TMPDIR` points under `target/`, and cargo creates it for the
    // integration tests. That is exactly the control we are looking for.
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}
