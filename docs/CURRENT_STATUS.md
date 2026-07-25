# Current Status

This file is the short source of truth after ADR-11. When it conflicts with historical PRDs or Phase 0 spikes, the order of authority is: code, status JSON files, ADR-11, then this file. Historical docs remain useful for intent and rationale, not for shipped scope.

## Shipped

- Runtime: Rust workspace with a headless `agent-core`, Ratatui TUI, headless `-p` mode, JSONL sessions, resume, and `/goal`.
- Provider: one wired adapter, `OpenAiChatGpt`, using the ChatGPT subscription channel through the Codex backend.
- Auth: OAuth PKCE flow and refresh-token rotation for the ChatGPT subscription, stored in the OS keyring.
- Tools: `read`, `glob`, `grep`, `write`, `edit`, and `bash`, with fail-closed tool metadata, permissions, taint propagation, and concurrent read dispatch.
- Sandbox: Linux Landlock filesystem confinement for the process tree, plus a cooperative local HTTP(S) proxy for subprocesses that honor `HTTP(S)_PROXY`.
- MCP: config loading, lifecycle state, stdio client plumbing, and tool listing. MCP tools are not yet exposed as callable model tools.
- Docs rename: `pyxis` is the public command and repo name. Internal crates still use `agent-*`.

## Deferred

- Public provider adapters: OpenAI BYOK, Anthropic, Gemini, OpenRouter, Ollama, Bedrock, Vertex, and Azure are architectural backlog, not shipped adapters.
- Public OpenAI Responses BYOK mode and server-side `previous_response_id` mode.
- MCP tools in the agent loop, stable connect UX, and per-server OAuth.
- Paneflow in-process embedding, GPU diff rendering, plan trees, and hunk review.
- Vector memory, sub-agents, prompt-cache strategy, VCR provider tests, packaged releases, macOS Seatbelt, and cross-platform hardening.

## Live Risks

- ChatGPT subscription auth is unofficial and revocable. It is a convenience channel, not a contractual foundation.
- The `originator=pyxis` rename validation still needs a live post-rename check against the ChatGPT backend.
- Network control is proxy-based and cooperative. It helps for HTTP(S) subprocesses, but it is not a kernel-level network sandbox and does not block raw sockets by itself.
- Linux is the only supported sandbox target today. Off-Linux filesystem confinement degrades explicitly.

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
