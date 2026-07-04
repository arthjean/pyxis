# Codex TUI Port Inventory

This inventory supports EP-001 of `tasks/prd-codex-tui-parity.md`. It records
which local Codex modules Pyxis may port, how each target is classified, and
which license obligations apply before any code is copied or adapted.

## Classification

| Class | Meaning | Rule |
|---|---|---|
| `copy` | Verbatim or near-verbatim source reuse. | Preserve Apache-2.0 notices before merge. |
| `adapt` | Source is structurally derived from Codex with Pyxis-specific edits. | Preserve Apache-2.0 provenance before merge. |
| `inspired` | Concepts and behavior are studied, but Pyxis code is written independently. | No copied source notice required, keep this inventory current. |
| `skip` | Out of scope, unclear provenance, or blocked until clarification. | Do not port until reclassified. |

## License Decision

Pyxis remains GPL-3.0-or-later. The Apache Software Foundation documents that
Apache-2.0 source can be included in GPLv3 projects, while the reverse direction
is not compatible for Apache projects. The FSF license list also marks
Apache-2.0 as compatible with GPLv3 and not GPLv2.

Operational obligations for this repo:

1. Keep `NOTICE-CODEX.md` when any Codex source is copied or adapted.
2. Preserve upstream Apache-2.0 license and notice text for copied or adapted
   files.
3. Mark any unclear provenance as `skip`.
4. Keep `agent-core` headless: no Ratatui, Crossterm, ANSI, or Codex UI code in
   the core crate.

Sources:

- ASF GPL compatibility: https://www.apache.org/licenses/GPL-compatibility.html
- GNU license list: https://www.gnu.org/licenses/license-list.html#apache2
- ASF license FAQ: https://www.apache.org/foundation/license-faq.html

## Feature Validation

The default rollback path must keep passing through the normal workspace gates.
The parity scaffold is behind `agent-tui/codex_tui_parity`, so EP-001 coverage
also requires:

```powershell
cargo test -p agent-tui --features codex_tui_parity
```

## Current Inventory

| Pyxis target | Codex source | Class | Notes |
|---|---|---|---|
| `crates/agent-tui/src/app_event.rs` | `C:\dev\codex\codex-rs\tui\src\app_event.rs`, `chatwidget\command_lifecycle.rs` | `adapt` | Pyxis keeps its own `AgentEvent` adapter, but the lifecycle split for active/finalized transcript items follows Codex. |
| `crates/agent-tui/src/history_cell.rs` | `C:\dev\codex\codex-rs\tui\src\history_cell\mod.rs`, `history_cell\messages.rs`, `history_cell\approvals.rs`, `history_cell\plans.rs`, `history_cell\search.rs`, `history_cell\separators.rs`, `history_cell\mcp.rs`, `history_cell\patches.rs`, `history_cell\request_user_input.rs`, `history_cell\notices.rs`, `history_cell\session.rs`, `history_cell\hook_cell.rs`, `exec_cell\model.rs`, `exec_cell\render.rs` | `adapt` | Pyxis keeps a monolithic file for now, but the `HistoryCell` raw/rich/transcript contract, approval cells, plan updates, web search, MCP calls, request-user-input results, special notices, patch summaries/failures, final separators, session headers, hook runs, exec transcript lines, and output limits are structurally derived from Codex. |
| `crates/agent-tui/src/streaming.rs` | `C:\dev\codex\codex-rs\tui\src\streaming\controller.rs` and `streaming\table_holdback.rs` | `inspired` | Stable-prefix and tail behavior is documented, not ported. |
| `crates/agent-tui/src/bottom_pane.rs` | `C:\dev\codex\codex-rs\tui\src\bottom_pane\mod.rs`, `bottom_pane_view.rs`, `approval_overlay.rs`, `list_selection_view.rs`, `chat_composer.rs` | `inspired` | EP-001 creates the gated module only. Full composer and views stay in later stories. |
| `crates/agent-tui/src/state.rs`, `crates/agent-cli/src/interactive.rs`, `crates/agent-cli/src/settings.rs`, `crates/agent-tools/src/registry.rs` | `C:\dev\codex\codex-rs\tui\src\chatwidget\permissions_menu.rs`, `permission_popups.rs`, `app\config_persistence.rs`, `app\event_dispatch.rs`, `status_surfaces.rs` | `inspired` | Pyxis mirrors the `/permissions` picker, history notice shape, runtime mode update, and user-level persistence, but maps it to Pyxis-owned `PermissionMode` values rather than Codex permission profiles. |
| `crates/agent-tui/src/render.rs` | `C:\dev\codex\codex-rs\tui\src\chatwidget\rendering.rs`, `C:\dev\codex\codex-rs\tui\src\render\renderable.rs`, `C:\dev\codex\codex-rs\tui\src\app.rs` | `inspired` | Parity layout borrows Codex's transcript and bottom-pane separation, but Pyxis intentionally keeps the composer anchored at the terminal bottom and renders the transcript tail above it. |
| `crates/agent-tui/src/state.rs`, `crates/agent-tui/src/render.rs`, `crates/agent-tui/src/term.rs`, `crates/agent-cli/src/interactive.rs` | `C:\dev\codex\codex-rs\tui\src\app.rs`, `chatwidget.rs`, `bottom_pane\mod.rs`, `bottom_pane\chat_composer.rs` | `inspired` | Pyxis mirrors Codex shutdown feedback for confirmed quit: the composer shows `Shutting down...`, footer hints are hidden, one final frame is drawn, then the terminal UI is cleared before restore. |
| `crates/agent-tui/src/insert_history.rs` | `C:\dev\codex\codex-rs\tui\src\insert_history.rs`, `terminal_hyperlinks.rs` | `adapt` | Inline insertion remains Pyxis-specific, but finalized history now preserves styled Ratatui lines instead of flattening to plain text. |
| `crates/agent-tui/src/terminal_hyperlinks.rs` | `C:\dev\codex\codex-rs\tui\src\terminal_hyperlinks.rs` | `adapt` | Simplified Pyxis implementation of Codex's separate visible text plus terminal hyperlink metadata model. |
| `crates/agent-tui/src/term.rs` | `C:\dev\codex\codex-rs\tui\src\tui.rs` | `inspired` | The parity path keeps native terminal scrollback available, clears startup build output from the visible screen, and uses a full-height inline viewport for Pyxis's bottom composer preference. |
| `crates/agent-tui/src/terminal_viewport.rs` | `C:\dev\codex\codex-rs\tui\src\tui.rs` and `insert_history.rs` | `inspired` | Viewport geometry scaffold only. |
| Future app-server runtime integration | `C:\dev\codex\codex-rs\tui\src\history_cell\hook_cell.rs`, app-server hook emitters | `skip` | Pyxis now owns hook transcript types and rendering. Direct app-server hook emission remains out of scope until Pyxis has equivalent runtime events. |
| Future app-server or connector surfaces | `C:\dev\codex\codex-rs\app-server\**`, connector crates, realtime/audio/pets/collab surfaces | `skip` | Explicit PRD non-goals. |
