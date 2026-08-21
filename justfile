# The quality gates of this repository, named.
#
# `check` is the verdict: the four `cargo` commands `.github/workflows/ci.yml`
# runs, in the same order. The CI does NOT call `just`; it keeps its own steps
# with their per-step `timeout`, their streaming filter and their step summary,
# because a job cancelled by `timeout-minutes` archives no log. Keeping the two
# inventories identical is therefore a requirement on every edit to either file.
#
# Only syntax available since `just` 1.0 is used here: no attribute, no
# `set unstable`. The packaged versions this file is expected to run on go back
# to 1.21.0 (Ubuntu 24.04 universe).

# List every recipe in this file; running `just` with no argument does this.
default:
    @just --list

# Check the formatting of the whole workspace, the cheapest gate.
fmt:
    cargo fmt --all -- --check

# Lint every target, tests included; no `-D warnings`, unwrap/expect are warn by decision.
lint:
    cargo clippy --workspace --all-targets

# Link the test binaries without running them, where the whole graph is codegened.
build-tests:
    cargo test --workspace --no-run

# Run the whole suite, naming every failing test instead of stopping at the first.
test:
    cargo test --workspace --no-fail-fast

# Run the four CI gates in order, stopping at the first one that fails.
check: fmt lint build-tests test

# `check` plus both parity gates; needs the pinned Codex clone, never runs in CI.
check-local: check
    cargo run -p agent-parity -- check
    # `-`: upstream moving is a report, not a verdict, so `drift` cannot turn this red.
    -cargo run -p agent-parity -- drift

# WRITES to the repository: regenerates schemas, snapshots and the parity matrix; read `git diff` after.
regen:
    PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas
    cargo insta review
    cargo run -p agent-parity -- generate
