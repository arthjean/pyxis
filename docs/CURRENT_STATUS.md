# Current Status

This file is the short source of truth after ADR-11. When it conflicts with historical PRDs or Phase 0 spikes, the order of authority is: code, status JSON files, ADR-11, then this file. Historical docs remain useful for intent and rationale, not for shipped scope.

## Shipped

- Runtime: Rust workspace with a headless `agent-core`, Ratatui TUI, headless `-p` mode, JSONL sessions, resume, and `/goal`.
- Provider: one wired adapter, `OpenAiChatGpt`, using the ChatGPT subscription channel through the Codex backend.
- Auth: OAuth PKCE flow and refresh-token rotation for the ChatGPT subscription, stored in the OS keyring.
- Tools: `read`, `glob`, `grep`, `write`, `edit`, and `bash`, with fail-closed tool metadata, permissions, taint propagation, and concurrent read dispatch.
- Sandbox: Linux Landlock filesystem confinement for the process tree, plus a cooperative local HTTP(S) proxy for subprocesses that honor `HTTP(S)_PROXY`. Writable roots cover the workspace, the temporary directory, and any extra roots declared in `writable_roots` of `~/.pyxis/settings.toml`; a root pointing at `/` or at the whole home is refused.
- MCP: config loading, lifecycle state, stdio client plumbing, tool listing, and tools callable by the model. Servers are connected at startup (no experimental flag), their tools enter the registry as `mcp__<server>__<tool>` with deterministic shortening under the 64-byte API limit and schemas rewritten into strict mode, every result is untrusted, every call asks by default, and a server declared by the workspace still requires `/mcp <server> trust`. Tools are registered at startup only: a mid-session connection changes the lifecycle, not what the model can call. Delivered by EP-003 of `tasks/prd-harness-capabilities.md`.
- Skills: `~/.agents/skills/<name>/SKILL.md` read per the open Agent Skills spec (restricted frontmatter reader, no YAML dependency: only the `name` and `description` scalars). Names and descriptions are injected per turn as user-level context under an explicit byte budget; `/<name>` injects the body of that skill for the turn, ephemeral like the project context, with the invocation readable in the persisted transcript. Angle brackets and control characters of the frontmatter are neutralized, a symlinked `SKILL.md` is refused, and an invalid skill is dropped with a trace. Deferred: the `scripts/`, `references/` and `assets/` directories of the spec. Delivered by EP-004 of `tasks/prd-harness-capabilities.md`.
- Hooks: `[[hooks]]` entries of `~/.pyxis/settings.toml` (global only) declare commands run around each tool call, on the Claude Code contract: JSON event on stdin (`hook_event_name`, `tool_name`, `tool_input`, `cwd`, plus a bounded `tool_response` for `PostToolUse`), JSON decision on stdout under `hookSpecificOutput.permissionDecision`, exit code 2 blocking with stderr carried to the model. Two deliberate deviations, both fail-closed: a hook can only tighten (`allow` reads as "no objection", never as a bypass of a confirmation or of the taint defense), and every failure denies (missing executable, 5-second timeout, non-zero exit, unreadable stdout, unknown decision). A `deny` outranks `BypassPermissions`; an `ask` forces a confirmation in every mode and is never rememberable. A hook is executed directly (no shell) and inherits the Landlock confinement of the process, so it cannot write outside the writable roots of the session. Without a declaration nothing is spawned, cloned, or delayed. Delivered by EP-005 of `tasks/prd-harness-capabilities.md`; `crates/agent-tools/src/hooks.rs`.
- Observability: a panic restores the terminal before printing anything, then appends a dated report (version, location, message) to `~/.pyxis/logs/panic.log` and names that path on stderr. `PYXIS_LOG=error|warn|info|debug|trace` installs a `tracing` subscriber writing to `~/.pyxis/logs/trace-<millis>.log`, one file per run. Both files are created before Landlock and granted individually, like `settings.toml`, since the confinement cannot open a path that does not exist yet. The crates only ever emit through the facade: no library writes on a process output any more, and no crate but the binary installs a subscriber. Message and tool-input content only appears at `trace`. Unset variable: no subscriber, no output, an atomic level check per emission point. Delivered by EP-006 of `tasks/prd-harness-capabilities.md`; `crates/agent-cli/src/observability.rs`.
- Docs rename: `pyxis` is the public command and repo name. Internal crates still use `agent-*`.
- Composer: multi-line input (Alt+Enter, Ctrl+J, Shift+Enter where the terminal reports it), wrapped rendering with a 10-line cap and vertical scrolling, and large pastes collapsed to a `[collage : N lignes]` summary that expands to the full content on submit. Delivered by EP-003 of `tasks/prd-harness-parity.md`; `crates/agent-tui/src/composer.rs` holds the wrap and cursor mapping.
- Configuration: TOML parsed by the reference library, with precedence defaults < `~/.pyxis/settings.toml` < `<workspace>/.pyxis/config.toml` < environment < command line. The project file cannot set `permission_mode`, `writable_roots` or `hooks`; those keys are dropped with a warning, and an unusable `[[hooks]]` entry is dropped on its own without taking the valid ones or the startup down. An invalid file names its line and key and starts on defaults instead of failing. Both modes read the configuration, so a global `permission_mode` now applies to `-p` as well, announced on stderr when it widens the headless default. Delivered by US-016 of `tasks/prd-harness-parity.md`.
- Machine output: `pyxis -p --output-format json` writes one JSON event per line, each carrying a schema version, ending with a `run_summary` line (session id, model turns, cumulative tokens, end cause, exit code). Schema in `docs/EVENT_SCHEMA.md`; the default text output is unchanged. Delivered by US-017.
- Turn diff: every turn exposes an aggregated diff of the files it touched, including files written by a `bash` command rather than by an edit tool, as the structured `AgentEvent::TurnDiff`. Rendered as a one-line summary in the TUI, emitted as `turn_diff` in the JSONL stream. Delivered by US-018; `crates/agent-tools/src/turn_diff.rs`.

## Deferred

- Public provider adapters: OpenAI BYOK, Anthropic, Gemini, OpenRouter, Ollama, Bedrock, Vertex, and Azure are architectural backlog, not shipped adapters.
- Public OpenAI Responses BYOK mode and server-side `previous_response_id` mode.
- Mid-session MCP tool registration, remote MCP transports, and per-server OAuth.
- Rich TUI: plan trees and hunk-level diff review.
- Vector memory, sub-agents, prompt-cache strategy, VCR provider tests, packaged releases, macOS Seatbelt, and cross-platform hardening.

## Live Risks

- ChatGPT subscription auth is unofficial and revocable. It is a convenience channel, not a contractual foundation.
- The `originator=pyxis` rename validation still needs a live post-rename check against the ChatGPT backend.
- Network control is proxy-based and cooperative. It helps for HTTP(S) subprocesses, but it is not a kernel-level network sandbox and does not block raw sockets by itself.
- Linux is the only supported sandbox target today. Off-Linux filesystem confinement degrades explicitly.
- Deferred-execution subpaths (`.git/` as a whole, which covers `hooks/`, `config`, and the worktree `gitdir:` pointer file, plus `.pyxis/`) are refused by the `write` and `edit` tools, before any permission decision, including through a symlink. That protection does **not** cover `bash`: Landlock rules are additive, so a write right granted on the workspace cannot be subtracted for a subpath, and a shell command can still write there. Closing that hole would mean changing the whole sandbox strategy, not patching a rule (US-013 of `tasks/prd-harness-parity.md`). That includes `.pyxis/config.toml`: a `bash` command can write the project configuration that the next launch reads, which is why no security key is ever honored from that file (US-016).
- The aggregated turn diff is scoped to what git reports as different from `HEAD`, untracked files included and ignored files excluded. In a directory that is not a git repository the turn diff is always empty. That is a deliberate answer to an open question of `tasks/prd-harness-parity.md`: fingerprinting the whole workspace would cost seconds per turn on a large repo, and watching the filesystem would add a dependency.

## Status Reconciliation (2026-07-25)

US-008 of `tasks/prd-harness-parity.md` re-checked every `DONE` story against
its acceptance criteria. Outcome:

- `tasks/prd-codex-tui-parity-status.json`: US-017 and US-018 moved back to
  `IN_REVIEW`, with the `path:line` proof recorded in each story's
  `review_note`. The composer was never ported (input is still a flat `String`
  submitted on Enter), and the repository held zero render snapshots while
  US-018 required at least twenty. EP-005 and the PRD status follow. US-001 to
  US-016 were confronted with their criteria and stay `DONE`.
- Snapshot coverage now exists (`crates/agent-tui/tests/snapshots/`, 27
  snapshots), delivered by US-005 and US-006 of `tasks/prd-harness-parity.md` —
  not by US-018. The story stays `IN_REVIEW` because its own deliverable, the
  app loop parity validation, was not what shipped.
- Sampled `DONE` stories from the archived PRDs (`prd-pyxis`,
  `prd-codex-orchestration`, `prd-response-rendering`) verify against the code:
  Landlock plus proxy sandbox (`crates/agent-sandbox/src/fs.rs`,
  `proxy.rs`), SSE connect and idle watchdog (`crates/agent-provider/src/chatgpt.rs`),
  four-pass fuzzy edit matching (`crates/agent-tools/src/edit.rs:196`), and the
  scroll pill (`crates/agent-tui/src/render.rs:767`). One naming divergence:
  `prd-codex-orchestration` US-025 names a `seek_sequence` helper that ships
  under a different name; the behavior it describes is implemented.
- One anomaly is recorded rather than fixed: `tasks/prd-pyxis-status.json`
  declares `prd.status = "READY"` while all of its stories are `DONE`,
  `CANCELLED`, or `DEFERRED`. That file is listed under **Files NOT to Modify**
  in `tasks/prd-harness-parity.md`, so it is documented here instead of being
  edited.
