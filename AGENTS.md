# AGENTS.md

Pyxis is a terminal AI coding agent written in Rust: a headless core plus thin
clients (Ratatui TUI, headless `-p`, app-server). Cargo workspace, Linux only,
Rust 1.95 / edition 2024. The published binary is `pyxis` and it is produced by
`crates/agent-cli`, the only crate that depends on everything.

## Authorization boundaries

- The Codex baseline clone resolved by `$PYXIS_CODEX_BASELINE` is **read only**.
  Never commit, checkout, fetch, or write anything inside it. `agent-parity`
  touches it with `git rev-parse HEAD` and file reads, nothing else.
- Moving a baseline is an explicit decision, never automatic upstream tracking.
  Two independent pins exist, each governing its own matrix: `BASELINE_COMMIT` in
  `crates/agent-parity/src/lib.rs` drives
  `docs/parity/codex-baseline-matrix.json`, and `BASELINE_COMMIT` in
  `crates/agent-parity/src/client_model.rs` drives
  `docs/parity/codex-client-model-matrix.json`. Change the one your work
  concerns, run `cargo run -p agent-parity -- generate`, and read the diff before
  committing.
- `PYXIS_LIVE_PARITY=1` spends the maintainer's ChatGPT subscription against a
  real OpenAI endpoint. Run it only when this session explicitly asks for a live
  run.
- Copying or adapting Codex source (Apache-2.0) carries obligations: list the
  file and its classification in `docs/codex-port-inventory.md`, keep the
  provenance in `NOTICE-CODEX.md`, and keep the result GPL-3.0-or-later.
- `spikes/` is a separate, excluded, throwaway Phase 0 workspace. Do not build,
  fix, or reuse it as part of MVP work.

## Build and verify

System prerequisites: `mold` (forced for the whole workspace by
`.cargo/config.toml`), `libdbus-1-dev`, `pkg-config`, plus `just`, which names
the gates below. `just` is a local runner only: the CI does not install it and
keeps its own `cargo` steps.

```bash
cargo build --workspace
cargo test --workspace
```

The gates of this repository are recipes, and `just --list` is their inventory.
The `justfile` carries the commands; `.github/workflows/ci.yml` runs the same
ones in the same order, and `cargo test -p agent-doc-gates` fails when the two
inventories drift apart.

| Aggregate | What it runs |
|---|---|
| `just check` | The verdict: the four gates of the CI, in order, stopping at the first failure |
| `just check-local` | `just check` plus both parity gates; needs the pinned Codex clone, never runs in CI, and upstream drift stays non-blocking |
| `just regen` | WRITES to the repository: schemas, snapshots, parity matrix. Read `git diff` afterwards, and never call it from a verification |

Targeted verification signals. The third column says which aggregate carries
the command, so no gate here is one nothing ever runs:

| Change | Command | In the aggregates |
|---|---|---|
| Anything touching Codex contract surface | `just parity` | `just check-local`, blocking |
| Checking what moved upstream | `just drift` | `just check-local`, kept non-blocking by the `-` sigil |
| App-server protocol types | `PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas` | a line of `just regen`; it writes, so no verification recipe may run it |
| TUI rendering | `cargo insta review` after `cargo test -p agent-tui` | the review is a line of `just regen` and is interactive; the test it reviews runs inside `just test` |
| Decision records, and the gate inventory the `justfile`, `.github/workflows/ci.yml`, `AGENTS.md` and `CONTRIBUTING.md` describe | `cargo test -p agent-doc-gates` | runs inside `just test` |
| Live parity against a real OpenAI endpoint | see `docs/parity/offline-suite.md` | outside every aggregate on purpose: it spends the maintainer's subscription, so no recipe may set `PYXIS_LIVE_PARITY` |

Adding, renaming, or moving a test named in the table of
`docs/parity/offline-suite.md` breaks `crates/agent-parity/tests/offline_suite.rs`,
which parses that table. Update the row in the same change.

## Invariants

`docs/ARCHITECTURE.md` closes with the full numbered list. Cargo proves the
dependency bans; these are the ones nothing mechanical catches:

- `agent-core` emits **only** structured `AgentEvent`. Never ANSI, never color,
  never layout. Provider `StreamEvent`s are consumed inside the core and never
  relayed to a client.
- Every crate emits observability through the `tracing` facade. Only the binary
  installs a subscriber and only the binary writes to stdout or stderr.
- Tool output is untrusted by default; the taint propagates and forces `Ask` on a
  destructive or network action.
- Trait `Tool` defaults are fail-closed.
- `run_agent` is the only model-tool engine. `agent-runtime` reaches it through
  `TurnRunner` and never reimplements retry, compaction, or dispatch.
- A turn produces exactly one terminal state, persisted before it is published.
- Withholding is not retry. Only context errors (PTL, max-tokens, `413`) feed
  `PendingError` and reactive compaction. `Retryable`, `Overloaded`, and
  `RateLimited` are absorbed by the transverse backoff and never enter
  `PendingError`.
- An accepted operation is durable before it is acknowledged. Resubmitting a
  `client_message_id` already accepted returns the original identifiers and
  re-executes nothing.
- One cancellation tree: every thread, turn, tool, and child is a CHILD node of
  the previous one. Cancellation descends, never climbs, and a client-side
  `JoinHandle::abort` is forbidden because it would cut the future between a
  `tool_use` and its result.
- Orchestration limits are crate constants. Do not add a public configuration key
  for them.

## Where new behavior goes

| Adding | Goes in |
|---|---|
| A tool | one module under `crates/agent-tools/src/`, registered through `Registry::register`. Fail-closed defaults stay unless the module argues otherwise |
| Something a client must see | a variant of `AgentEvent` in `crates/agent-core/src/event.rs`, never a relayed provider `StreamEvent` |
| A provider | a module under `crates/agent-provider/src/` behind the `Provider` trait. The core does not change |
| An orchestration limit | a constant in the owning crate. Never a configuration key |
| A user-facing setting | `crates/agent-cli/src/settings.rs`, placed in the `ConfigLayer` precedence |
| An app-server method | `crates/agent-app-server/src/protocol.rs`, then regenerate the schemas |
| Turn or thread lifecycle | `crates/agent-runtime/src/` through `TurnRunner`. Never a second model-tool loop |
| Rendering | `crates/agent-tui/`, proved by a reviewed snapshot |

## Source of truth

Order of authority when documents disagree: the code, then `docs/DECISIONS.md`,
then `docs/CURRENT_STATUS.md`. No single ADR arbitrates: ADR-9 fixes the error
taxonomy, ADR-11 the current MVP scope, ADR-12 the thread runtime, ADR-13 the
subagent NO-GO. `docs/parity/audits/`, the decision notes under `docs/notes/`, and
`docs/ROADMAP.md` are historical context for intent and rationale; they no longer
arbitrate shipped scope.

The boundary between the two registers is a test, not a taste: a decision belongs in
`docs/DECISIONS.md` when a future change to the shipped crates can violate it, and in the
note tree when nothing in `crates/` can. ADR-12 fixes how `agent-runtime` reaches
`run_agent` and a pull request can break it, so it is an ADR; not starting from the Codex
base is a path already taken that no crate contradicts, so it is a note. An ADR gets no
mirror note, and a note that touches one links it. When the two disagree the ADR wins and
the note is stale: correct it or move it to `rejected/`. The reciprocal rule and the format
the gate enforces live in [`docs/notes/README.md`](docs/notes/README.md).

The normative parity artifacts are `docs/parity/codex-baseline-matrix.json` and
`docs/parity/codex-client-model-matrix.json`. Both are generated and
fingerprinted, never hand-edited.

## Conventions

- Language split: code, comments, doc comments, `README.md`, `CONTRIBUTING.md`,
  `docs/CURRENT_STATUS.md`, and commit messages are **English**. The architecture
  documents under `docs/` are **French**. Match the file you are editing.
- Test functions are full sentences in snake_case naming the behavior proved:
  `a_poll_returns_only_what_came_after_the_previous_chunk`, not `test_poll`.
- `panic!`, `unimplemented!`, and `dbg!` are denied; `unwrap`/`expect` are
  warnings. Prefer `?`, `ok_or(...)`, `match`. Tests may use them
  (`clippy.toml`); a test file that needs `panic!` as its reporting mechanism
  states so with a file-level `#![allow(...)]` and a comment saying why.
- A dependency that is not self-evident is argued in a comment above its entry in
  the workspace `Cargo.toml`: the feature that forces it, the version constraint
  that matters, the alternative rejected. Follow that shape rather than adding a
  bare line.
- Commit bodies are prose that explains the failure mode the change removes, not
  a bullet list of edits. Read `git log` for the register before writing one.

## Where to read more

| Topic | Document |
|---|---|
| Invariants, agent loop, tools, MCP, sessions | `docs/ARCHITECTURE.md` |
| Crate graph: the sixteen crates, their role and their internal edges, generated | [`docs/crate-graph.md`](docs/crate-graph.md) |
| Catalogue de configuration: the fifteen keys, their layer, flag, variable and security character, generated | [`docs/config-catalog.md`](docs/config-catalog.md) |
| ADRs and structural decisions | `docs/DECISIONS.md` |
| Shipped scope, deferred work, live risks | `docs/CURRENT_STATUS.md` |
| Provider contract and error taxonomy | `docs/PROVIDERS.md` |
| Headless JSONL event contract | `docs/EVENT_SCHEMA.md` |
| Parity baseline and offline proof recipe | `docs/parity/README.md` |
| App-server protocol | `docs/app-server/README.md` |
| Decision notes: tree, format, and ADR boundary | [`docs/notes/README.md`](docs/notes/README.md) |
