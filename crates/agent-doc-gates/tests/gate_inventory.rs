//! The non-drift gate (US-085, US-086): the aggregate runs what the CI runs.
//!
//! The repository keeps its gates twice, once as recipes and once as workflow
//! steps, and the workflow keeps its own steps on purpose: a job cancelled by
//! `timeout-minutes` archives no log, so the steps must fail from the inside,
//! with their `timeout`, their streaming filter and their step summary. What the
//! duplication costs is paid here. `CONTRIBUTING.md` had already drifted to
//! `cargo clippy --no-deps` against the workflow's `--all-targets` with nothing
//! to notice, and a `just check` that no longer runs the CI's commands would be
//! worse: it would answer green on a change the CI refuses.
//!
//! This gate runs no process, reads no environment variable and has no
//! conditional skip. It reads two text files and returns the same verdict on a
//! runner without `just` installed, which is the only reason it can live inside
//! `cargo test --workspace`.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a divergence must
//! stop the suite with the file, the gate and the expected command in the
//! message, and `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    AGGREGATE_RECIPE, GATE_MARKER, Gate, JUSTFILE, WORKFLOW, check_gate_documents, check_gates,
    compare_gates, justfile_gates, repository_root, workflow_gates,
};

/// A recipe file holding the four gates of the workflow below, used as the
/// neighbor of whatever a fixture is proving.
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

# Needs the pinned clone.
check-local: check
    cargo run -p agent-parity -- check
    -cargo run -p agent-parity -- drift
";

/// A workflow holding the same four gates, one wrapped in `timeout`, one buried
/// in a block scalar, plus two steps running no cargo at all.
const WORKFLOW_YAML: &str = "\
name: CI

jobs:
  check:
    steps:
      - uses: actions/checkout@v5

      - name: Install system dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends libdbus-1-dev pkg-config mold

      - name: Format
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --workspace --all-targets

      - name: Build tests
        run: timeout 25m cargo test --workspace --no-run

      - name: Tests
        id: tests
        run: |
          set -o pipefail
          status=0
          timeout --kill-after=1m 10m \\
            cargo test --workspace --no-fail-fast 2>&1 \\
            | tee cargo-test.log \\
            | grep --line-buffered -E '^(test result:)' \\
            || status=$?
          if [ \"$status\" -ne 0 ]; then
            # The full log has to reach the archive whenever the gate is red.
            cat cargo-test.log
          fi
          exit \"$status\"

      - name: Report failing tests
        if: failure()
        run: |
          echo \"--- full cargo output ---\"
          sed -n 's/^test \\(.*\\) FAILED$/- `\\1`/p' cargo-test.log >> \"$GITHUB_STEP_SUMMARY\"
";

/// The same workflow with one more gate, the shape a lot that adds a check to
/// the CI alone produces.
fn workflow_with_added_gate() -> String {
    format!(
        "{WORKFLOW_YAML}\n      - name: Release build\n        run: cargo build --workspace --release\n"
    )
}

fn steps_of(gates: &[Gate]) -> Vec<&str> {
    gates.iter().map(|gate| gate.step.as_str()).collect()
}

fn commands_of(gates: &[Gate]) -> Vec<String> {
    gates.iter().map(|gate| gate.argv.join(" ")).collect()
}

// US-086: the real files of this repository.

#[test]
fn the_justfile_and_the_workflow_of_this_repository_run_the_same_gates_in_the_same_order() {
    let violations = check_gates(&repository_root());
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn the_gates_of_this_repository_are_the_four_cargo_commands_of_its_workflow() {
    let workflow = std::fs::read_to_string(repository_root().join(WORKFLOW))
        .expect("the workflow of this repository is readable");
    let gates = workflow_gates(&workflow).expect("the workflow parses");
    assert_eq!(
        steps_of(&gates),
        vec!["Format", "Clippy", "Build tests", "Tests"]
    );
    assert_eq!(
        commands_of(&gates),
        vec![
            "cargo fmt --all -- --check",
            "cargo clippy --workspace --all-targets",
            "cargo test --workspace --no-run",
            "cargo test --workspace --no-fail-fast",
        ]
    );
}

// US-085: what each extraction returns.

#[test]
fn each_extraction_returns_its_gates_paired_by_step_name_and_in_file_order() {
    let recipes = justfile_gates(RECIPES).expect("the recipes parse");
    let workflow = workflow_gates(WORKFLOW_YAML).expect("the workflow parses");
    assert_eq!(steps_of(&recipes), steps_of(&workflow));
    assert_eq!(commands_of(&recipes), commands_of(&workflow));
    assert!(check_gate_documents(RECIPES, WORKFLOW_YAML).is_empty());
}

#[test]
fn a_timeout_wrapper_and_its_options_are_removed_before_the_comparison() {
    let workflow = workflow_gates(WORKFLOW_YAML).expect("the workflow parses");
    let build = workflow
        .iter()
        .find(|gate| gate.step == "Build tests")
        .expect("the wrapped step is a gate");
    assert_eq!(build.argv.join(" "), "cargo test --workspace --no-run");
    let tests = workflow
        .iter()
        .find(|gate| gate.step == "Tests")
        .expect("the block-scalar step is a gate");
    assert_eq!(
        tests.argv.join(" "),
        "cargo test --workspace --no-fail-fast",
        "the shell plumbing after `2>&1` is not part of the command"
    );
}

#[test]
fn a_wrapper_other_than_timeout_fails_the_extraction_and_names_itself() {
    let wrapped = WORKFLOW_YAML.replace(
        "run: timeout 25m cargo test --workspace --no-run",
        "run: nice -n 10 cargo test --workspace --no-run",
    );
    let errors = workflow_gates(&wrapped).expect_err("an unknown wrapper is refused");
    let reported = errors.first().expect("one error");
    assert!(reported.contains("nice"), "{reported}");
    assert!(reported.contains("Build tests"), "{reported}");
    assert!(reported.contains("timeout"), "{reported}");
}

#[test]
fn a_workflow_step_running_no_cargo_command_is_ignored_without_an_error() {
    let gates = workflow_gates(WORKFLOW_YAML).expect("the workflow parses");
    assert!(!steps_of(&gates).contains(&"Install system dependencies"));
    assert!(!steps_of(&gates).contains(&"Report failing tests"));
    assert_eq!(gates.len(), 4, "only the four cargo steps are gates");
}

#[test]
fn a_marker_matching_no_workflow_step_is_reported_as_orphaned() {
    let recipes = RECIPES.replace("# ci-step: Clippy", "# ci-step: Lints");
    let violations = check_gate_documents(&recipes, WORKFLOW_YAML);
    let reported = violations
        .iter()
        .find(|violation| violation.contains("orphelin"))
        .expect("the orphaned marker is reported");
    assert!(reported.contains("Lints"), "{reported}");
    assert!(reported.contains(JUSTFILE), "{reported}");
}

#[test]
fn a_marked_recipe_running_no_cargo_command_is_reported_by_its_name() {
    let recipes = RECIPES.replace("    cargo fmt --all -- --check", "    echo formatted");
    let errors = justfile_gates(&recipes).expect_err("a marked recipe without a gate is refused");
    let reported = errors.first().expect("one error");
    assert!(reported.contains("fmt"), "{reported}");
    assert!(reported.contains("Format"), "{reported}");
}

#[test]
fn a_marked_recipe_whose_failure_is_ignored_is_reported_because_a_ci_step_cannot_be_silent() {
    let recipes = RECIPES.replace(
        "    cargo clippy --workspace --all-targets",
        "    -cargo clippy --workspace --all-targets",
    );
    let errors = justfile_gates(&recipes).expect_err("a non-blocking gate is refused");
    let reported = errors.first().expect("one error");
    assert!(reported.contains("lint"), "{reported}");
    assert!(reported.contains("non bloquant"), "{reported}");
}

#[test]
fn the_unmarked_recipes_are_not_gates_so_the_parity_commands_stay_out_of_the_comparison() {
    let gates = justfile_gates(RECIPES).expect("the recipes parse");
    for gate in &gates {
        assert!(
            !gate.argv.contains(&"agent-parity".to_string()),
            "{:?}",
            gate.argv
        );
    }
}

// US-086: what a divergence reports.

#[test]
fn a_gate_added_to_the_workflow_alone_names_the_step_and_the_command_to_add() {
    let violations = check_gate_documents(RECIPES, &workflow_with_added_gate());
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("Release build"), "{reported}");
    assert!(
        reported.contains("cargo build --workspace --release"),
        "{reported}"
    );
    assert!(reported.contains(GATE_MARKER), "{reported}");
    assert!(reported.contains(JUSTFILE), "{reported}");
}

#[test]
fn a_flag_changed_on_one_side_alone_is_reported_with_both_invocations() {
    let recipes = RECIPES.replace(
        "cargo clippy --workspace --all-targets",
        "cargo clippy --workspace --no-deps",
    );
    let violations = check_gate_documents(&recipes, WORKFLOW_YAML);
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("Clippy"), "{reported}");
    assert!(reported.contains("--all-targets"), "{reported}");
    assert!(reported.contains("--no-deps"), "{reported}");
    assert!(reported.contains(WORKFLOW), "{reported}");
}

#[test]
fn swapping_two_recipes_is_reported_as_an_order_divergence_and_not_as_a_missing_gate() {
    let swapped = RECIPES
        .replace("# ci-step: Format", "# ci-step: PLACEHOLDER")
        .replace("# ci-step: Clippy", "# ci-step: Format")
        .replace("# ci-step: PLACEHOLDER", "# ci-step: Clippy");
    let violations = check_gate_documents(&swapped, WORKFLOW_YAML);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("ordre")),
        "{violations:?}"
    );
    assert!(
        violations
            .iter()
            .all(|violation| !violation.contains("absente")),
        "{violations:?}"
    );
}

#[test]
fn reordering_the_aggregate_alone_is_reported_because_it_changes_what_runs_first() {
    let recipes = RECIPES.replace(
        "check: fmt lint build-tests test",
        "check: lint fmt build-tests test",
    );
    let violations = check_gate_documents(&recipes, WORKFLOW_YAML);
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("one violation");
    assert!(reported.contains("lint fmt build-tests test"), "{reported}");
    assert!(reported.contains("fmt lint build-tests test"), "{reported}");
}

#[test]
fn every_divergence_is_reported_at_once_instead_of_stopping_at_the_first() {
    let recipes = RECIPES.replace(
        "cargo clippy --workspace --all-targets",
        "cargo clippy --workspace --no-deps",
    );
    let violations = check_gate_documents(&recipes, &workflow_with_added_gate());
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Release build")),
        "{violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("--no-deps")),
        "{violations:?}"
    );
    for reported in &violations {
        assert_eq!(reported.lines().count(), 1, "{reported}");
        assert!(reported.starts_with("gates: "), "{reported}");
    }
}

#[test]
fn the_gate_launches_no_process_and_reads_no_environment_variable_so_it_never_skips() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gates.rs"),
    )
    .expect("the module source is readable");
    for forbidden in ["std::process", "Command", "env::var", "env!"] {
        assert!(
            !source.contains(forbidden),
            "the non-drift gate must stay a pure read of two text files: {forbidden}"
        );
    }
}

#[test]
fn a_gate_added_to_the_justfile_alone_names_the_recipe_and_the_command_no_step_runs() {
    let recipes = RECIPES.replace(
        "# The verdict.\ncheck: fmt lint build-tests test",
        "# ci-step: Audit\n# A gate the workflow never got.\naudit:\n    cargo audit\n\n\
         # The verdict.\ncheck: fmt lint build-tests test audit",
    );
    let left = justfile_gates(&recipes).expect("the recipes parse");
    let right = workflow_gates(WORKFLOW_YAML).expect("the workflow parses");
    let violations = compare_gates(&left, &right);
    let reported = violations
        .iter()
        .find(|violation| violation.contains("cargo audit"))
        .expect("the recipe no step runs is reported");
    assert!(reported.contains("Audit"), "{reported}");
    assert!(reported.contains(JUSTFILE), "{reported}");
    assert!(reported.contains(WORKFLOW), "{reported}");
    assert!(check_gate_documents(&recipes, WORKFLOW_YAML).len() >= violations.len());
}

#[test]
fn a_recipe_file_without_its_aggregate_is_reported_because_nothing_composes_the_gates() {
    let recipes = RECIPES.replace("check: fmt lint build-tests test", "gates: fmt lint");
    let violations = check_gate_documents(&recipes, WORKFLOW_YAML);
    let reported = violations
        .iter()
        .find(|violation| violation.contains(AGGREGATE_RECIPE))
        .expect("the missing aggregate is reported");
    assert!(reported.contains(JUSTFILE), "{reported}");
}
