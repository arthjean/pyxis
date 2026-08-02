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

As of the composer and status-line rework (2026-07-31), the TUI adds one
structurally derived boundary and no verbatim reuse. The baseline is the
read-only clone at `/home/arthur/dev/codex`, commit
`f0c30e528a54bdf0fa9a4d52ff74b34383434811`.

Structurally derived, written against Pyxis types:

- `crates/agent-tui/src/footer.rs`, plus `render_input` and `footer_props` in
  `crates/agent-tui/src/render.rs`, from
  `codex-rs/tui/src/bottom_pane/footer.rs`,
  `bottom_pane/chat_composer.rs` (its `layout_areas`, `desired_height`,
  `render_with_mask`, `footer_mode` and `handle_shortcut_overlay_key`),
  `bottom_pane/status_line_style.rs` and `ui_consts.rs`. The `FooterMode`
  waterfall, the two-column gutter shared by composer and footer, the ` · `
  status line with per-category accents, the right-aligned mode indicator, the
  collapse order and the `?` shortcut overlay are adapted; every line is
  written against Pyxis state and its palette. The composer keeps its own
  full-width rules rather than Codex's blank framing. Divergences are listed
  in `docs/codex-port-inventory.md`.

As of the chat-surface rework (2026-07-31), the TUI adds one **MIT** derivation
and four structurally derived boundaries. The Codex baseline is unchanged
(`f0c30e528a54bdf0fa9a4d52ff74b34383434811`).

Derived from Ratatui (MIT, not Apache-2.0), with its licence text preserved in
the file header:

- `crates/agent-tui/src/custom_terminal.rs`, from `ratatui::Terminal` and
  `ratatui::Frame` (ratatui 0.29.0). Ratatui freezes the height of a
  `Viewport::Inline` at construction and exposes no way to change it, so the
  parity renderer cannot size the drawn area to its content. The derivation
  exists to make `viewport_area` writable; double buffering, `Buffer::diff` and
  cursor handling stay upstream's. Codex derived the same type for the same
  reason (`codex-rs/tui/src/custom_terminal.rs`), which is where the approach
  comes from; the Pyxis file is written from the ratatui source, not from
  Codex's.
- The row-splitting strategy of `crates/agent-tui/src/insert_history.rs`, from
  `ratatui::Terminal::insert_before` (its `scrolling-regions` path): paint into
  rows the viewport gives up while it has any, then scroll the rows above it
  into the scrollback. Divergence: upstream pushes the viewport down towards a
  screen bottom it does not occupy, where the Pyxis viewport is already anchored
  there and yields rows from its top instead.

Structurally derived from Codex, written against Pyxis types:

- `crates/agent-tui/src/parse_command.rs`, from
  `codex-rs/shell-command/src/parse_command.rs` and the `ParsedCommand` shape in
  `codex-rs/protocol/src/parse_command.rs`. The Read/ListFiles/Search/Unknown
  classification, the shell-wrapper unwrapping and the pipeline collapse rule
  are adapted; the tokenizer, the redirection and command-substitution guard,
  and the command tables are Pyxis's own.
- The pacing policy in `crates/agent-tui/src/streaming.rs`, from
  `codex-rs/tui/src/streaming/chunking.rs` and `streaming/commit_tick.rs`. The
  Smooth/CatchUp two-gear model, its hysteresis and the thresholds are adapted;
  the queue lives inside `StreamController` rather than in a separate
  `StreamState`.
- `ExecCell`'s `Explored` grouping in `crates/agent-tui/src/history_cell.rs`,
  from `codex-rs/tui/src/exec_cell/{model,render}.rs`. Divergence: a failed call
  leaves the group so its error stays visible, where Codex keeps it.
- The viewport anchoring in `crates/agent-tui/src/term.rs` and the resize
  reflow in `ChatSurface::reflow`, from `codex-rs/tui/src/tui.rs` (`draw`) and
  `codex-rs/tui/src/app/resize_reflow.rs`. Divergence: the Pyxis viewport always
  reaches the last row of the screen, so the composer never drifts up; history
  takes rows back from the viewport's top. Codex lets its viewport follow the end
  of the transcript instead. Second divergence: the reflow reclaims only
  the rows this session wrote and still shows, where Codex purges the whole
  scrollback with `ESC[3J`. Purging would also erase what the user had in their
  terminal before Pyxis started.
- The OSC 8 marking in `crates/agent-tui/src/insert_history.rs`, from the
  hyperlink handling of `codex-rs/tui/src/insert_history.rs`: the destination is
  folded into the cell symbol, which carries no display width, because the write
  path is a cell diff with no room for out-of-band output.

Reference source inventory: `docs/codex-port-inventory.md`.

As of EP-006 of `tasks/prd-parite-client-modele-codex-api.md` (2026-08-02),
the canonical auxiliary contracts and provider clients study the read-only Codex baseline at commit
`ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff`. No module is structurally
transplanted. Endpoint names, JSON field names, Realtime event tags, the
`sediment://` URI prefix and the two model-facing Realtime V2 tool descriptions
are retained as wire contracts; the object-safe capability surface, typed
errors, bounded transport, redaction, durable-before-memory seam, closed
Realtime dialect model and conformance harness are Pyxis implementations. The
two tool descriptions are the only verbatim source
text retained for this epic.
The exact source paths and divergences are recorded in
`docs/codex-port-inventory.md`.
