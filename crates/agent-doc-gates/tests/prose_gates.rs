//! The prose gate (US-088): the prescriptive documents name recipes.
//!
//! The divergence this refuses is one the repository actually produced.
//! `CONTRIBUTING.md` prescribed `cargo clippy --workspace --no-deps` while
//! `.github/workflows/ci.yml` ran `--all-targets`, and nothing noticed for as
//! long as both were prose: `--no-deps` never compiles the test targets, so a
//! lint inside a `#[cfg(test)]` passed locally and turned the pull request red on
//! a gate its author had run green. Correcting the sentence buys nothing on its
//! own, a second correction being one edit away, so the rule is mechanical: an
//! invocation that shares a gate's head and diverges from it fails
//! `cargo test --workspace`, and the message says which recipe to write instead.
//!
//! The scope is two files and it is closed. `docs/parity/offline-suite.md`
//! publishes a normative recipe a reader is meant to copy, `README.md` shows
//! session transcripts: both legitimately carry invocations, and a wider rule
//! would forbid the two places where writing one is the point.
//!
//! `panic!` through `assert!` is the reporting mechanism here, and
//! `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    JUSTFILE, PROSE_DOCUMENTS, check_prose_documents, check_prose_gates, repository_root,
};

/// The recipe file reduced to what the prose gate reads: the marked gates and
/// the aggregate composing them.
const RECIPES: &str = "\
# ci-step: Format
# The cheapest gate.
fmt:
    cargo fmt --all -- --check

# ci-step: Clippy
# Every target, tests included.
lint:
    cargo clippy --workspace --all-targets

# ci-step: Build tests
# Where the graph is codegened.
build-tests:
    cargo test --workspace --no-run

# ci-step: Tests
# The whole suite.
test:
    cargo test --workspace --no-fail-fast

# The verdict.
check: fmt lint build-tests test
";

/// A prescriptive document written the way this lot leaves them: recipes named,
/// the shorter `cargo` path offered to a contributor without `just`, targeted
/// commands untouched.
const CONVERGED: &str = "\
# Contributing

- Build and test the whole workspace:
  ```bash
  cargo build --workspace
  cargo test --workspace
  ```
- Run `just check` before pushing; `just --list` names every other recipe.
- Regenerate the schemas with `PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas`.
- The decision records are proved by `cargo test -p agent-doc-gates`.
";

fn violations_of(document: &str) -> Vec<String> {
    check_prose_documents(RECIPES, &[("CONTRIBUTING.md", document)])
}

// The real documents of this repository.

#[test]
fn the_prescriptive_documents_of_this_repository_name_recipes_instead_of_gate_invocations() {
    let violations = check_prose_gates(&repository_root());
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn contributing_still_offers_the_plain_cargo_path_to_a_contributor_without_just() {
    let contributing = std::fs::read_to_string(repository_root().join("CONTRIBUTING.md"))
        .expect("CONTRIBUTING.md is readable");
    assert!(contributing.contains("just check"), "{contributing}");
    assert!(
        contributing.contains("cargo test --workspace"),
        "the plain path stays written"
    );
}

#[test]
fn agents_names_the_three_aggregates_and_sends_the_reader_to_the_inventory() {
    let agents = std::fs::read_to_string(repository_root().join("AGENTS.md"))
        .expect("AGENTS.md is readable");
    for cited in [
        "just check",
        "just check-local",
        "just regen",
        "just --list",
    ] {
        assert!(agents.contains(cited), "AGENTS.md never names `{cited}`");
    }
}

#[test]
fn agents_names_the_regeneration_command_of_each_catalog_and_the_gate_stays_green() {
    let agents = std::fs::read_to_string(repository_root().join("AGENTS.md"))
        .expect("AGENTS.md is readable");
    for command in [
        "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-doc-gates --test crate_graph",
        "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis tool_catalog",
        "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis config_catalog",
    ] {
        assert!(
            agents.contains(command),
            "AGENTS.md never names `{command}`"
        );
    }
    // A targeted `cargo test -p …` shares no head with `cargo test --workspace`,
    // which is the only reason a regeneration command can be written out here.
    assert!(check_prose_gates(&repository_root()).is_empty());
}

#[test]
fn a_regeneration_command_colliding_with_a_gate_names_the_line_of_agents_and_the_recipes() {
    let violations = check_prose_documents(
        RECIPES,
        &[(
            "AGENTS.md",
            "# Pyxis\n\nRégénérer par `PYXIS_UPDATE_CATALOGS=1 cargo test --workspace --catalogs`.\n",
        )],
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("AGENTS.md:3"), "{reported}");
    assert!(reported.contains("--catalogs"), "{reported}");
    assert!(reported.contains("just build-tests"), "{reported}");
    assert!(reported.contains("just test"), "{reported}");
}

// What the rule accepts.

#[test]
fn a_converged_document_carries_no_violation() {
    assert!(
        violations_of(CONVERGED).is_empty(),
        "{:?}",
        violations_of(CONVERGED)
    );
}

#[test]
fn the_shorter_workspace_command_stays_allowed_because_it_contradicts_no_gate() {
    let violations = violations_of("Run `cargo test --workspace` for the whole suite.\n");
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn a_targeted_command_is_not_a_formulation_of_a_gate() {
    let violations = violations_of(
        "Run `cargo test -p agent-doc-gates` and `cargo insta review` after `cargo test -p agent-tui`.\n",
    );
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn prose_that_merely_mentions_a_command_outside_a_code_span_is_not_read_as_one() {
    let violations = violations_of("The CI runs cargo clippy --workspace --no-deps nowhere.\n");
    assert!(violations.is_empty(), "{violations:?}");
}

// What the rule refuses, and what it tells the reader to write.

#[test]
fn the_clippy_formulation_that_drifted_once_names_the_file_the_line_and_the_recipe() {
    let violations = violations_of(
        "# Contributing\n\nRun `cargo clippy --workspace --no-deps` before pushing.\n",
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("CONTRIBUTING.md:3"), "{reported}");
    assert!(reported.contains("--no-deps"), "{reported}");
    assert!(reported.contains("--all-targets"), "{reported}");
    assert!(reported.contains("just lint"), "{reported}");
    assert!(reported.contains("just check"), "{reported}");
}

#[test]
fn a_gate_written_verbatim_is_refused_because_the_recipe_is_the_single_source() {
    let violations = violations_of("```bash\ncargo fmt --all -- --check\n```\n");
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("CONTRIBUTING.md:2"), "{reported}");
    assert!(reported.contains("Format"), "{reported}");
    assert!(reported.contains("just fmt"), "{reported}");
}

#[test]
fn a_head_shared_by_two_gates_cites_both_recipes_rather_than_guessing() {
    let violations = violations_of("Run `cargo test --workspace --release` sometimes.\n");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("just build-tests"), "{reported}");
    assert!(reported.contains("just test"), "{reported}");
}

#[test]
fn every_violation_of_a_document_is_reported_at_once() {
    let violations = violations_of(
        "```bash\ncargo fmt --all -- --check\ncargo clippy --workspace --no-deps\ncargo test --workspace --no-fail-fast\n```\n",
    );
    assert_eq!(violations.len(), 3, "{violations:?}");
    for reported in &violations {
        assert_eq!(reported.lines().count(), 1, "{reported}");
        assert!(reported.starts_with("gates: "), "{reported}");
        assert!(
            reported.contains("écrire « just"),
            "le message dit quoi écrire : {reported}"
        );
    }
}

// The closed scope.

#[test]
fn the_scope_is_the_two_prescriptive_documents_and_nothing_else() {
    assert_eq!(PROSE_DOCUMENTS, ["AGENTS.md", "CONTRIBUTING.md"]);
}

#[test]
fn the_normative_recipe_of_the_offline_suite_is_out_of_scope_and_keeps_its_invocations() {
    let published = std::fs::read_to_string(repository_root().join("docs/parity/offline-suite.md"))
        .expect("the offline suite recipe is readable");
    assert!(
        published.contains("cargo test --workspace --no-fail-fast"),
        "the published recipe is meant to be copied verbatim"
    );
    // Held against the rule it would fail it: the offline suite is out of scope
    // because the scope excludes it, not because it happens to be clean.
    let justfile = std::fs::read_to_string(repository_root().join(JUSTFILE))
        .expect("the justfile is readable");
    let held = check_prose_documents(&justfile, &[("docs/parity/offline-suite.md", &published)]);
    assert!(
        !held.is_empty(),
        "the published recipe writes the gates out, so only the closed scope spares it"
    );
    assert!(
        !PROSE_DOCUMENTS.contains(&"docs/parity/offline-suite.md"),
        "the gate never reads it"
    );
}
