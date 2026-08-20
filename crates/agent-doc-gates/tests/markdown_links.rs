//! The link gate (US-051): a relative link of this repository resolves on disk.
//!
//! The note tree encodes the lifecycle in the directory, so a note changes status
//! by changing path and every link aimed at the old path dies without a sound.
//! That is the one real cost of the two-axis scheme, and this gate is how it is
//! paid: the suite fails on a dead target instead of a reader tripping on it six
//! months later. It also guards the migration of the dated documents that used to
//! sit at the root of `docs/`.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a violation must
//! stop `cargo test --workspace` with the source, the line, and the target in the
//! message, and `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    DOCS_ROOT, check_links, markdown_documents, relative_links, repository_root,
};
use std::fs;
use std::path::{Path, PathBuf};

/// A scratch repository under the integration-test temp directory Cargo provides,
/// so no fixture needs a temp-file dependency the lot forbids.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(DOCS_ROOT)).expect("the scratch repository is created");
    dir.canonicalize().expect("the scratch repository resolves")
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture directory is created");
    }
    fs::write(path, content).expect("the fixture file is written");
}

#[test]
fn every_relative_link_of_this_repository_resolves_to_an_existing_file() {
    let errors = check_links(&repository_root());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn the_gate_reads_the_root_markdown_files_alongside_the_documentation_tree() {
    let documents = markdown_documents(&repository_root());
    let named: Vec<String> = documents
        .iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    assert!(named.iter().any(|name| name == "AGENTS.md"), "{named:?}");
    assert!(named.iter().any(|name| name == "README.md"), "{named:?}");
    assert!(
        documents
            .iter()
            .any(|path| path.ends_with("docs/notes/README.md")),
        "the note tree specification is covered"
    );
}

#[test]
fn a_relative_link_to_a_missing_file_is_reported_with_its_source_line_and_target() {
    let root = scratch("missing-target");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\nUne phrase.\n\nVoir [la suite](./disparu.md).\n",
    );
    let errors = check_links(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("docs/depart.md:5"), "{reported}");
    assert!(reported.contains("./disparu.md"), "{reported}");
    assert!(reported.contains("introuvable"), "{reported}");
    assert_eq!(reported.lines().count(), 1, "{reported}");
}

#[test]
fn an_absolute_link_to_an_external_site_is_ignored_without_any_network_access() {
    let root = scratch("external-link");
    write(
        &root,
        "docs/sources.md",
        "# Sources\n\n[MADR](https://adr.github.io/madr/) et [le mainteneur](mailto:a@b.c).\n",
    );
    assert!(check_links(&root).is_empty());
    // Nothing resolvable is even extracted: the gate never holds a target it
    // could only answer over the network.
    let extracted = relative_links("[MADR](https://adr.github.io/madr/)");
    assert!(extracted.is_empty(), "{extracted:?}");
}

#[test]
fn a_link_to_an_anchor_of_an_existing_file_is_checked_on_the_file_alone() {
    let root = scratch("anchored-link");
    write(&root, "docs/cible.md", "# Cible\n\n## Une section\n");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n[La section](./cible.md#une-section-qui-nexiste-pas).\n",
    );
    assert!(check_links(&root).is_empty());
}

#[test]
fn a_link_that_is_only_an_anchor_of_the_current_document_is_ignored() {
    let root = scratch("self-anchor");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n[Plus bas](#plus-bas)\n\n## Plus bas\n",
    );
    assert!(check_links(&root).is_empty());
}

#[test]
fn a_link_inside_a_fenced_block_is_an_example_and_not_a_target() {
    let root = scratch("fenced-link");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n```markdown\n[un gabarit](./nulle-part.md)\n```\n",
    );
    assert!(check_links(&root).is_empty());
}

#[test]
fn a_link_carrying_a_title_resolves_on_its_target_alone() {
    let root = scratch("titled-link");
    write(&root, "docs/cible.md", "# Cible\n");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n[Cible](./cible.md \"le titre du lien\")\n",
    );
    assert!(check_links(&root).is_empty());
}

#[test]
fn a_link_climbing_out_of_the_repository_is_reported_by_its_own_rule() {
    let root = scratch("escaping-link");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n[Ailleurs](../../../etc/hosts)\n",
    );
    let errors = check_links(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    let reported = errors.first().expect("one error");
    assert!(reported.contains("sort du dépôt"), "{reported}");
}

#[test]
fn a_link_resolves_relatively_to_the_document_that_carries_it() {
    let root = scratch("nested-link");
    write(&root, "docs/parity/README.md", "# Parité\n");
    write(&root, "docs/parity/audits/mesure.md", "# Mesure\n");
    write(
        &root,
        "docs/parity/audits/renvoi.md",
        "# Renvoi\n\n[La maison](../README.md) et [la mesure](./mesure.md).\n",
    );
    write(
        &root,
        "AGENTS.md",
        "# Guide\n\n[La mesure](docs/parity/audits/mesure.md)\n",
    );
    assert!(check_links(&root).is_empty(), "{:?}", check_links(&root));
}

#[test]
fn every_dead_link_is_reported_instead_of_stopping_at_the_first() {
    let root = scratch("all-dead-links");
    write(&root, "docs/vivant.md", "# Vivant\n");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n[Un](./mort-un.md)\n\n[Deux](./mort-deux.md)\n\n[Trois](./vivant.md)\n",
    );
    write(
        &root,
        "README.md",
        "# Racine\n\n[Quatre](./docs/mort-trois.md)\n",
    );
    let errors = check_links(&root);
    assert_eq!(errors.len(), 3, "{}", errors.join("\n"));
    for reported in &errors {
        assert!(reported.starts_with("lien: "), "{reported}");
        assert_eq!(reported.lines().count(), 1, "{reported}");
    }
}

#[test]
fn an_image_pointing_at_a_missing_asset_is_reported_like_any_other_link() {
    let root = scratch("missing-image");
    write(
        &root,
        "docs/depart.md",
        "# Départ\n\n![Un schéma](./schema.svg)\n",
    );
    let errors = check_links(&root);
    assert_eq!(errors.len(), 1, "{}", errors.join("\n"));
    assert!(
        errors.first().expect("one error").contains("./schema.svg"),
        "{errors:?}"
    );
}

#[test]
fn a_destination_carrying_balanced_parentheses_is_read_whole() {
    let extracted = relative_links("[Une page](./un(deux).md) et [autre](./autre.md)");
    assert_eq!(
        extracted,
        vec![
            (1, "./un(deux).md".to_string()),
            (1, "./autre.md".to_string())
        ]
    );
}

#[test]
fn the_gate_returns_the_same_verdict_whatever_the_working_directory() {
    // `repository_root()` is derived from the manifest, never from the current
    // directory, so a `cargo test` launched from a crate subdirectory reads the
    // same tree.
    let root = repository_root();
    assert!(root.join(DOCS_ROOT).is_dir(), "{}", root.display());
    assert_eq!(check_links(&root), check_links(&root.join(".").join("")));
}
