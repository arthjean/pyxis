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

Reference source inventory: `docs/codex-port-inventory.md`.
