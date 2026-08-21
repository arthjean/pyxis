//! The crate-graph gate (US-093, US-094): the published graph is what the
//! sixteen manifests say, and nothing on disk escapes it.
//!
//! `README.md` and `docs/ARCHITECTURE.md` each published an exhaustive table of
//! crates and each was wrong, by six and by five, because a table written by
//! hand goes stale one line per new crate and nothing ever says so. The document
//! this gate guards is rendered instead, and the gate has two halves that catch
//! two different failures: the byte comparison catches an edited or outdated
//! file, and the completeness guard catches a generator that forgot a crate,
//! which the comparison alone would happily call fresh.
//!
//! Regenerate with:
//!
//! ```text
//! PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-doc-gates --test crate_graph
//! ```
//!
//! The gate launches no process, opens no socket and reads one environment
//! variable, the write switch. That is the whole reason it can live inside
//! `cargo test --workspace` and return the same verdict on a runner without
//! `just`, without a Codex clone and without a network.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a stale document
//! must stop the suite with its path and the regeneration command in the
//! message, and `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    CRATE_GRAPH_DOC, CRATES_ROOT, CrateManifest, GENERATOR, NO_DEPENDENCY, REGENERATE_COMMAND,
    UPDATE_VARIABLE, check_crate_graph, check_crate_graph_completeness, collect_manifests,
    crate_directories, crate_graph_document, parse_manifest, render_crate_graph, rendered_crates,
    repository_root,
};
use std::fs;
use std::path::{Path, PathBuf};

/// A scratch tree under the integration-test temp directory Cargo provides, so
/// no fixture needs the temp-file dependency this crate forbids.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(CRATES_ROOT)).expect("the scratch tree is created");
    dir.canonicalize().expect("the scratch tree resolves")
}

fn write_manifest(root: &Path, name: &str, content: &str) {
    let dir = root.join(CRATES_ROOT).join(name);
    fs::create_dir_all(&dir).expect("the fixture crate directory is created");
    fs::write(dir.join("Cargo.toml"), content).expect("the fixture manifest is written");
}

fn manifest(name: &str, description: &str, dependencies: &[&str]) -> CrateManifest {
    CrateManifest {
        name: name.to_string(),
        description: description.to_string(),
        internal_dependencies: dependencies.iter().map(|entry| entry.to_string()).collect(),
    }
}

// US-094: the published document of this repository.

#[test]
fn the_published_crate_graph_matches_the_manifests_of_the_workspace() {
    let root = repository_root();
    let path = root.join(CRATE_GRAPH_DOC);
    let expected = match crate_graph_document(&root) {
        Ok(rendered) => rendered,
        Err(errors) => panic!("{}", errors.join("\n")),
    };
    if std::env::var_os(UPDATE_VARIABLE).is_some() {
        fs::write(&path, &expected).expect("the published graph is writable");
        return;
    }
    // An absent file is a stale file: the remedy is the same command, so there
    // is no reason to report it differently.
    let found = fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        found,
        expected,
        "{} is stale; regenerate with {REGENERATE_COMMAND}",
        path.display()
    );
}

#[test]
fn every_crate_directory_of_this_repository_appears_in_the_published_graph() {
    let root = repository_root();
    let published = fs::read_to_string(root.join(CRATE_GRAPH_DOC)).unwrap_or_default();
    let violations = check_crate_graph(&root, &published);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn the_graph_covers_the_sixteen_crates_and_names_the_ones_the_prose_tables_missed() {
    let root = repository_root();
    let listed = rendered_crates(&crate_graph_document(&root).expect("the graph renders"));
    assert_eq!(listed, crate_directories(&root.join(CRATES_ROOT)));
    for missed in [
        "agent-app-server",
        "agent-code-mode",
        "agent-code-mode-v8",
        "agent-doc-gates",
        "agent-parity",
        "agent-runtime",
    ] {
        assert!(
            listed.iter().any(|name| name == missed),
            "{missed} was absent from a published table"
        );
    }
}

#[test]
fn the_header_names_its_generator_and_the_command_that_rewrites_the_document() {
    let rendered = crate_graph_document(&repository_root()).expect("the graph renders");
    let mut lines = rendered.lines();
    let first = lines.next().expect("a first line");
    let second = lines.next().expect("a second line");
    assert!(
        first.starts_with("<!--") && first.contains(GENERATOR),
        "{first}"
    );
    assert!(
        second.starts_with("<!--") && second.contains(REGENERATE_COMMAND),
        "{second}"
    );
}

#[test]
fn the_gate_launches_no_process_and_reads_no_environment_variable_of_its_own() {
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/crate_graph.rs"))
            .expect("the module source is readable");
    for forbidden in ["std::process", "Command", "env::var", "env!"] {
        assert!(
            !source.contains(forbidden),
            "the crate-graph generator must stay a pure read of the manifests: {forbidden}"
        );
    }
}

// US-093: what the render produces.

#[test]
fn the_render_is_byte_identical_across_two_consecutive_runs_on_the_same_tree() {
    let root = repository_root();
    let first = crate_graph_document(&root).expect("the graph renders");
    let second = crate_graph_document(&root).expect("the graph renders again");
    assert_eq!(first, second);
    assert!(first.ends_with('\n'), "the document ends with one newline");
    assert!(
        !first.contains('\r'),
        "the document carries LF endings only"
    );
    assert!(
        !first.contains(root.to_string_lossy().as_ref()),
        "no absolute path reaches the render"
    );
}

#[test]
fn the_crates_and_their_edges_are_sorted_whatever_the_order_they_were_collected_in() {
    let unordered = [
        manifest("agent-tui", "Le frontend.", &["agent-core"]),
        manifest("agent-core", "Le cœur.", &[]),
        manifest("agent-cli", "Le binaire.", &["agent-tui", "agent-core"]),
    ];
    let mut reversed: Vec<CrateManifest> = unordered.to_vec();
    reversed.reverse();
    let left = render_crate_graph(&unordered).expect("the graph renders");
    let right = render_crate_graph(&reversed).expect("the graph renders");
    assert_eq!(left, right);
    assert_eq!(
        rendered_crates(&left),
        vec!["agent-cli", "agent-core", "agent-tui"]
    );
    let edges: Vec<&str> = left
        .lines()
        .filter(|line| line.starts_with("    ") && line.contains(" --> "))
        .map(str::trim)
        .collect();
    assert_eq!(
        edges,
        vec![
            "agent_cli --> agent_core",
            "agent_cli --> agent_tui",
            "agent_tui --> agent_core",
        ]
    );
}

#[test]
fn a_crate_without_any_internal_dependency_keeps_its_node_and_gets_an_absence_marker() {
    let rendered = render_crate_graph(&[
        manifest("agent-core", "Le cœur.", &["agent-tokenizer"]),
        manifest("agent-tokenizer", "Le comptage.", &[]),
    ])
    .expect("the graph renders");
    assert!(
        rendered.contains(&format!(
            "| `agent-tokenizer` | Le comptage. | {NO_DEPENDENCY} |"
        )),
        "{rendered}"
    );
    assert!(
        rendered.contains("    agent_tokenizer[\"agent-tokenizer\"]"),
        "a crate with no edge still has a node: {rendered}"
    );
}

#[test]
fn an_edge_aimed_at_a_crate_the_workspace_does_not_hold_fails_the_render() {
    let errors = render_crate_graph(&[manifest("agent-core", "Le cœur.", &["agent-absent"])])
        .expect_err("a dangling edge is refused");
    let reported = errors.first().expect("one error");
    assert!(reported.contains("agent-absent"), "{reported}");
    assert!(reported.contains("agent-core"), "{reported}");
}

#[test]
fn an_empty_workspace_fails_the_render_instead_of_publishing_an_empty_graph() {
    let errors = render_crate_graph(&[]).expect_err("an empty graph is refused");
    assert!(
        errors.first().expect("one error").contains(CRATES_ROOT),
        "{errors:?}"
    );
}

#[test]
fn a_pipe_in_a_description_is_escaped_so_the_table_survives_it() {
    let rendered = render_crate_graph(&[manifest("agent-core", "Un | deux.", &[])])
        .expect("the graph renders");
    let row = rendered
        .lines()
        .find(|line| line.starts_with("| `agent-core`"))
        .expect("the row is rendered");
    assert_eq!(row, "| `agent-core` | Un \\| deux. | aucune |");
}

// US-093: what the parser reads, and what it refuses.

#[test]
fn the_edges_come_from_the_dependency_sections_and_never_from_the_development_ones() {
    let parsed = parse_manifest(
        "crates/agent-tools/Cargo.toml",
        "[package]\n\
         name = \"agent-tools\"\n\
         description = \"Le registre.\"\n\
         \n\
         [dependencies]\n\
         agent-core.workspace = true\n\
         agent-runtime = { workspace = true }\n\
         regex.workspace = true\n\
         \n\
         [target.'cfg(unix)'.dependencies]\n\
         agent-sandbox.workspace = true\n\
         nix.workspace = true\n\
         \n\
         [dev-dependencies]\n\
         agent-parity.workspace = true\n\
         \n\
         [build-dependencies]\n\
         agent-tui.workspace = true\n",
    )
    .expect("the manifest parses");
    assert_eq!(
        parsed.internal_dependencies,
        vec!["agent-core", "agent-runtime", "agent-sandbox"]
    );
    assert_eq!(parsed.description, "Le registre.");
}

#[test]
fn a_dependency_entry_the_parser_cannot_read_fails_with_its_file_and_its_line() {
    let errors = parse_manifest(
        "crates/agent-core/Cargo.toml",
        "[package]\n\
         name = \"agent-core\"\n\
         description = \"Le cœur.\"\n\
         \n\
         [dependencies]\n\
         agent-tokenizer.workspace = true\n\
         serde = {\n\
         version = \"1\"\n\
         }\n",
    )
    .expect_err("a multi-line inline table is refused");
    let reported = errors.first().expect("one error");
    assert!(
        reported.contains("crates/agent-core/Cargo.toml:7"),
        "{reported}"
    );
    assert!(reported.contains("illisible"), "{reported}");
    assert_eq!(reported.lines().count(), 1, "{reported}");
}

#[test]
fn a_manifest_without_a_description_names_the_file_and_says_where_the_role_lives() {
    let errors = parse_manifest(
        "crates/agent-neuf/Cargo.toml",
        "[package]\nname = \"agent-neuf\"\nversion = \"0.0.0\"\n",
    )
    .expect_err("a manifest without a role is refused");
    let reported = errors.first().expect("one error");
    assert!(
        reported.contains("crates/agent-neuf/Cargo.toml"),
        "{reported}"
    );
    assert!(reported.contains("description"), "{reported}");
}

#[test]
fn a_comment_and_a_blank_line_inside_a_dependency_section_are_not_entries() {
    let parsed = parse_manifest(
        "crates/agent-core/Cargo.toml",
        "[package]\n\
         name = \"agent-core\"\n\
         description = \"Le cœur.\"\n\
         \n\
         [dependencies]\n\
         # Un argument sur la ligne suivante.\n\
         \n\
         agent-tokenizer.workspace = true\n",
    )
    .expect("the manifest parses");
    assert_eq!(parsed.internal_dependencies, vec!["agent-tokenizer"]);
}

#[test]
fn the_same_dependency_declared_twice_produces_one_edge() {
    let parsed = parse_manifest(
        "crates/agent-cli/Cargo.toml",
        "[package]\n\
         name = \"agent-cli\"\n\
         description = \"Le binaire.\"\n\
         \n\
         [dependencies]\n\
         agent-core.workspace = true\n\
         \n\
         [target.'cfg(unix)'.dependencies]\n\
         agent-core.workspace = true\n",
    )
    .expect("the manifest parses");
    assert_eq!(parsed.internal_dependencies, vec!["agent-core"]);
}

// US-094: the completeness guard, which freshness alone cannot provide.

#[test]
fn a_crate_directory_absent_from_the_rendered_graph_is_reported_by_its_name() {
    let rendered =
        render_crate_graph(&[manifest("agent-core", "Le cœur.", &[])]).expect("the graph renders");
    let violations = check_crate_graph_completeness(
        &rendered,
        &["agent-core".to_string(), "agent-oublie".to_string()],
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("agent-oublie"), "{reported}");
    assert!(reported.contains("Cargo.toml"), "{reported}");
}

#[test]
fn a_crate_of_the_rendered_graph_without_a_directory_is_reported_too() {
    let rendered = render_crate_graph(&[
        manifest("agent-core", "Le cœur.", &[]),
        manifest("agent-fantome", "Un crate supprimé.", &[]),
    ])
    .expect("the graph renders");
    let violations = check_crate_graph_completeness(&rendered, &["agent-core".to_string()]);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations
            .first()
            .expect("one violation")
            .contains("agent-fantome"),
        "{violations:?}"
    );
}

#[test]
fn an_unreadable_or_empty_crates_directory_fails_the_guard_instead_of_validating_nothing() {
    let root = scratch("empty-crates");
    assert!(crate_directories(&root.join(CRATES_ROOT)).is_empty());
    let violations = check_crate_graph(&root, "");
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations
            .first()
            .expect("one violation")
            .contains(CRATES_ROOT),
        "{violations:?}"
    );
    let errors = collect_manifests(&root.join(CRATES_ROOT))
        .expect_err("an empty workspace has no graph to collect");
    assert!(errors.first().expect("one error").contains(CRATES_ROOT));
}

#[test]
fn a_crate_added_to_the_disk_enters_the_render_without_touching_the_generator() {
    let root = scratch("added-crate");
    write_manifest(
        &root,
        "agent-core",
        "[package]\nname = \"agent-core\"\ndescription = \"Le cœur.\"\n",
    );
    write_manifest(
        &root,
        "agent-neuf",
        "[package]\n\
         name = \"agent-neuf\"\n\
         description = \"Le crate d'après.\"\n\
         \n\
         [dependencies]\n\
         agent-core.workspace = true\n",
    );
    let rendered = crate_graph_document(&root).expect("the graph renders");
    assert_eq!(rendered_crates(&rendered), vec!["agent-core", "agent-neuf"]);
    assert!(rendered.contains("Le crate d'après."), "{rendered}");
    assert!(check_crate_graph(&root, &rendered).is_empty());
}

#[test]
fn a_directory_without_a_manifest_is_not_a_crate_and_does_not_fail_the_guard() {
    let root = scratch("stray-directory");
    write_manifest(
        &root,
        "agent-core",
        "[package]\nname = \"agent-core\"\ndescription = \"Le cœur.\"\n",
    );
    fs::create_dir_all(root.join(CRATES_ROOT).join("brouillon"))
        .expect("the stray directory is created");
    assert_eq!(
        crate_directories(&root.join(CRATES_ROOT)),
        vec!["agent-core"]
    );
    let rendered = crate_graph_document(&root).expect("the graph renders");
    assert!(check_crate_graph(&root, &rendered).is_empty());
}
