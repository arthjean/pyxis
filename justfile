# The quality gates of this repository, named.
#
# `check` is the verdict: the four `cargo` commands `.github/workflows/ci.yml`
# runs, in the same order. The CI does NOT call `just`; it keeps its own steps
# with their per-step `timeout`, their streaming filter and their step summary,
# because a job cancelled by `timeout-minutes` archives no log. Keeping the two
# inventories identical is therefore a requirement on every edit to either file.
#
# `# ci-step: <name>` above a recipe pairs it with the workflow step of that
# name. It sits above the documentation comment so `just --list` keeps showing
# the line below it. `agent-doc-gates` reads both files and fails
# `cargo test --workspace` when the marked recipes and the workflow steps stop
# carrying the same commands in the same order.
#
# Only syntax available since `just` 1.0 is used here: no attribute, no
# `set unstable`. The packaged versions this file is expected to run on go back
# to 1.21.0 (Ubuntu 24.04 universe).

# List every recipe in this file; running `just` with no argument does this.
default:
    @just --list

# ci-step: Format
# Check the formatting of the whole workspace, the cheapest gate.
fmt:
    cargo fmt --all -- --check

# ci-step: Clippy
# Lint every target, tests included; no `-D warnings`, unwrap/expect are warn by decision.
lint:
    cargo clippy --workspace --all-targets

# ci-step: Build tests
# Link the test binaries without running them, where the whole graph is codegened.
build-tests:
    cargo test --workspace --no-run

# ci-step: Tests
# Run the whole suite, naming every failing test instead of stopping at the first.
test:
    cargo test --workspace --no-fail-fast

# Run the four CI gates in order, stopping at the first one that fails.
check: fmt lint build-tests test

# Compare the frozen parity matrices to the pinned Codex clone; reads it, never writes.
parity:
    cargo run -p agent-parity -- check

# Report what moved upstream since the pinned commit; exits non-zero by design when it did.
drift:
    cargo run -p agent-parity -- drift

# `check` plus both parity gates; needs the pinned Codex clone, never runs in CI.
check-local: check parity
    # `-`: upstream moving is a report, not a verdict, so `drift` cannot turn this red.
    -just drift

# WRITES to the repository: regenerates schemas, snapshots, the parity matrix and the three catalogs; read `git diff` after.
regen:
    PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas
    cargo insta review
    cargo run -p agent-parity -- generate
    PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-doc-gates --test crate_graph
    PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis tool_catalog
    PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis config_catalog
