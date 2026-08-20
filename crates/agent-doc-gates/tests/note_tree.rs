//! The structure gate (US-048): where a decision record is allowed to sit.
//!
//! The tree encodes the lifecycle in the directory, which is what makes a
//! declared status that contradicts its location impossible. That property is
//! worth nothing unless the two axes themselves are closed sets, so this gate
//! reads `docs/notes/` and fails the suite on a first-level directory that is
//! not a lifecycle, a class outside the six, a file at the wrong depth, an
//! undated filename, or a centralized index.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a violation must
//! stop `cargo test --workspace` with the offending path and the rule in the
//! message, and `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{CLASSES, LIFECYCLES, notes_root, walk_notes};
use std::fs;
use std::path::{Path, PathBuf};

/// A scratch tree under the integration-test temp directory Cargo provides, so
/// no fixture needs a temp-file dependency the lot forbids.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the scratch tree is created");
    dir
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture directory is created");
    }
    fs::write(path, content).expect("the fixture file is written");
}

/// A note whose format is beside the point: this gate only reads paths.
const BODY: &str = "# Note: exemple\n\nStatut: implemented\n\n## Problème\n";

fn errors_of(root: &Path) -> Vec<String> {
    walk_notes(root).1
}

#[test]
fn the_repository_note_tree_has_no_structure_violation() {
    let (_, errors) = walk_notes(&notes_root());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn an_absent_tree_yields_no_note_and_no_error() {
    let missing = scratch("absent-tree").join("nowhere");
    let (notes, errors) = walk_notes(&missing);
    assert!(notes.is_empty());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn an_empty_tree_yields_no_note_and_no_error() {
    let root = scratch("empty-tree");
    let (notes, errors) = walk_notes(&root);
    assert!(notes.is_empty());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn an_unknown_first_level_directory_is_reported_with_the_allowed_lifecycles() {
    let root = scratch("unknown-lifecycle");
    write(&root, "draft/process/2026-08-20-sujet.md", BODY);
    let errors = errors_of(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("draft/"), "{reported}");
    assert!(reported.contains("cycle de vie"), "{reported}");
    for lifecycle in LIFECYCLES {
        assert!(reported.contains(lifecycle), "{reported}");
    }
}

#[test]
fn an_unknown_class_directory_is_reported_with_the_allowed_classes() {
    let root = scratch("unknown-class");
    write(&root, "implemented/refactor/2026-08-20-sujet.md", BODY);
    let (notes, errors) = walk_notes(&root);
    assert!(notes.is_empty());
    let reported = errors.first().expect("one error");
    assert!(
        reported.contains("implemented/refactor/2026-08-20-sujet.md"),
        "{reported}"
    );
    assert!(reported.contains("refactor"), "{reported}");
    for class in CLASSES {
        assert!(reported.contains(class), "{reported}");
    }
}

#[test]
fn a_note_at_a_depth_other_than_lifecycle_class_file_is_reported_with_the_observed_depth() {
    let root = scratch("wrong-depth");
    write(&root, "implemented/2026-08-20-a-plat.md", BODY);
    write(&root, "proposed/process/sous/2026-08-20-trop-loin.md", BODY);
    let errors = errors_of(&root);
    assert_eq!(errors.len(), 2, "{}", errors.join("\n"));
    let flat = errors
        .iter()
        .find(|reported| reported.contains("implemented/2026-08-20-a-plat.md"))
        .expect("the shallow file is reported");
    assert!(flat.contains("profondeur observée : 2"), "{flat}");
    let deep = errors
        .iter()
        .find(|reported| reported.contains("2026-08-20-trop-loin.md"))
        .expect("the deep file is reported");
    assert!(deep.contains("profondeur observée : 4"), "{deep}");
}

#[test]
fn a_filename_without_a_date_prefix_is_reported_and_kept_out_of_the_valid_notes() {
    let root = scratch("undated-filename");
    write(&root, "implemented/process/note-sur-le-cache.md", BODY);
    let (notes, errors) = walk_notes(&root);
    assert!(notes.is_empty(), "an undated file is not a note");
    let reported = errors.first().expect("one error");
    assert!(reported.contains("note-sur-le-cache.md"), "{reported}");
    assert!(reported.contains("aaaa-mm-jj-sujet.md"), "{reported}");
}

#[test]
fn a_centralized_index_at_the_tree_root_is_reported_by_its_own_rule() {
    let root = scratch("centralized-index");
    write(&root, "INDEX.md", "# index\n");
    let errors = errors_of(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("INDEX.md"), "{reported}");
    assert!(reported.contains("index centralisé interdit"), "{reported}");
}

#[test]
fn the_readme_is_the_only_file_allowed_at_the_tree_root() {
    let root = scratch("root-files");
    write(&root, "README.md", "# spécification\n");
    write(&root, "brouillon.md", "# brouillon\n");
    let errors = errors_of(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("brouillon.md"), "{reported}");
}

#[test]
fn a_file_that_is_not_markdown_is_reported() {
    let root = scratch("non-markdown");
    write(&root, "implemented/process/2026-08-20-sujet.txt", BODY);
    let errors = errors_of(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("fichiers .md"), "{reported}");
}

#[test]
fn every_violation_is_reported_instead_of_stopping_at_the_first() {
    let root = scratch("all-violations");
    write(&root, "INDEX.md", "# index\n");
    write(&root, "draft/process/2026-08-20-sujet.md", BODY);
    write(&root, "implemented/refactor/2026-08-20-sujet.md", BODY);
    write(&root, "implemented/process/sans-date.md", BODY);
    write(&root, "rejected/2026-08-20-a-plat.md", BODY);
    write(&root, "proposed/process/2026-08-20-valide.md", BODY);
    let (notes, errors) = walk_notes(&root);
    assert_eq!(errors.len(), 5, "{}", errors.join("\n"));
    assert_eq!(notes.len(), 1, "the valid note survives its neighbors");
    for reported in &errors {
        assert_eq!(reported.lines().count(), 1, "{reported}");
        assert!(reported.starts_with("structure: "), "{reported}");
    }
}

#[test]
fn a_valid_note_carries_its_lifecycle_class_and_date() {
    let root = scratch("valid-note");
    write(
        &root,
        "implemented/architecture/2026-08-20-le-sujet.md",
        BODY,
    );
    let (notes, errors) = walk_notes(&root);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    let note = notes.first().expect("one note");
    assert_eq!(note.lifecycle, "implemented");
    assert_eq!(note.class, "architecture");
    assert_eq!(note.date, "2026-08-20");
    assert_eq!(note.rel, "implemented/architecture/2026-08-20-le-sujet.md");
}

#[test]
fn the_crate_declares_no_dependency_so_no_other_agent_crate_can_enter_its_graph() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("this crate's manifest is readable");
    let declared: Vec<&str> = manifest
        .lines()
        .skip_while(|line| line.trim() != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect();
    assert!(
        declared.is_empty(),
        "agent-doc-gates must stay dependency-free: {declared:?}"
    );
}
