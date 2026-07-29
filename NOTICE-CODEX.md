# Codex Source Notice

Pyxis is licensed under GPL-3.0-or-later.

The Codex TUI parity migration studies the local OpenAI Codex source tree at
`C:\dev\codex`, which is distributed under Apache License 2.0. Apache-2.0
source can be incorporated into GPLv3 projects, but only with license and
notice preservation.

As of the transcript rendering migration on 2026-07-03, Pyxis has structurally
adapted selected Codex TUI rendering concepts and module boundaries. No
verbatim source has been copied, but adapted files and provenance are tracked in
`docs/codex-port-inventory.md`. If a future story copies or further adapts
Codex source, the changed file must:

1. Keep the relevant Apache-2.0 provenance in this notice or a file-level header.
2. List the source path and classification in `docs/codex-port-inventory.md`.
3. Preserve any upstream copyright or notice text that applies to the copied or
   adapted source.
4. Keep the resulting Pyxis distribution under GPL-3.0-or-later.

As of EP-002 of `tasks/prd-parite-totale-codex-cli.md` (2026-07-28), the Code
Mode work adds one piece of VERBATIM Apache-2.0 reuse and three structurally
derived boundaries. The baseline is the read-only clone at
`/home/arthur/dev/codex`, commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`.

Verbatim reuse:

- The Lark grammar of the `exec` freeform tool and the `// @exec:` pragma
  prefix, in `crates/agent-code-mode/src/tools.rs`, from
  `codex-rs/core/src/tools/code_mode/execute_spec.rs` and
  `codex-rs/code-mode-protocol/src/description.rs`. They are reused unchanged
  ON PURPOSE: they are the wire a Codex-trained model emits, so paraphrasing
  them would break interoperability rather than protect anything.

Structurally derived, written against Pyxis types:

- `crates/agent-code-mode/src/{protocol,session}.rs`, from
  `codex-rs/code-mode-protocol/src/{session,runtime,response}.rs`.
- `crates/agent-code-mode-v8/src/{lib,engine,globals}.rs`, from
  `codex-rs/code-mode/src/{v8_init,runtime/mod,runtime/globals}.rs`.
- `spikes/s6-code-mode-v8/**`, from `codex-rs/v8-poc/src/lib.rs`.

As of EP-003 of the same PRD (2026-07-29), the multi-agent v2 work adds one
verbatim element and one structurally derived boundary, against the same
baseline commit.

Verbatim reuse:

- The six v2 tool NAMES and their argument names (`spawn_agent(task_name,
  message)`, `send_message(target, message)`, `followup_task(target, message)`,
  `list_agents(path_prefix)`, `wait_agent(timeout_ms)`,
  `interrupt_agent(target)`), in `crates/agent-tools/src/agent.rs`, from
  `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`. Reused unchanged ON
  PURPOSE: they are the surface a Codex-trained model calls, so renaming them
  would break interoperability. The descriptions, the schemas and every
  behaviour behind them are Pyxis's own.

Structurally derived, written against Pyxis types:

- `crates/agent-runtime/src/path.rs` (canonical task names), from
  `codex-rs/protocol/src/agent_path.rs`: the `/root/<name>` shape and the
  segment validation rules are the baseline's, the type and its bounds are
  written here.

As of EP-005 of the same PRD (2026-07-29), the app-server adds verbatim wire
NAMES and no code, against the same baseline commit.

Verbatim reuse:

- The JSON-RPC method and notification NAMES of the P0 subset, in
  `crates/agent-app-server/src/protocol.rs`, from
  `codex-rs/app-server-protocol/src/protocol/common.rs`: `initialize`,
  `thread/start`, `thread/resume`, `thread/unsubscribe`, `thread/items/list`,
  `turn/start`, `turn/steer`, `turn/interrupt`, `thread/started`,
  `thread/closed`, `turn/started`, `turn/completed`, `item/started`,
  `item/completed`, `item/agentMessage/delta`,
  `item/commandExecution/outputDelta`, `serverRequest/resolved`, `error`,
  `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`
  and `item/tool/call`. Reused unchanged ON PURPOSE: they are the contract a
  Codex-compatible client speaks. The payloads, the dispatcher, the item
  projection, the ownership rules, the back-pressure and both transports are
  Pyxis's own.

Nothing structural is derived: `agent-app-server` was written against the
acceptance criteria of EP-005 and the method table the US-001 verifier extracts
from the read-only clone, not against the upstream implementation.

As of EP-006 of the same PRD (2026-07-29), the observability and parity-proof
work adopts **nothing** from Codex, verbatim or structural. That is a finding
worth recording rather than an omission: the failure taxonomy, the trace
correlation, the offline coverage recipe and the drift verifier answer Pyxis
criteria (a shared cause category across four surfaces, opt-in-only remote
export, a pinned baseline that never follows HEAD) that have no upstream
counterpart. The only Codex artifacts involved are the ones already recorded:
the baseline clone, read but never written, and the contract matrix
`agent-parity` derives from it.

Reference source inventory: `docs/codex-port-inventory.md`.
