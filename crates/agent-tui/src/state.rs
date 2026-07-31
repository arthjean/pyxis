//! Client-side render state (US-019). `AppState` consumes the core's `AgentEvent`
//! (never ANSI) and files them into typed `Block`s; rendering (`render.rs`)
//! alone decides the presentation. Key handling returns an `InputAction`
//! that the agent-cli loop interprets (submit, permission, quit, scroll).

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agent_core::message::{ContentBlock, Message, Role, ToolCallId, ToolErrorKind};
use agent_core::{AgentEvent, TurnDiffView};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::footer::FooterMode;
use crate::measure;

/// A transcript item. Rendering picks weight/tint; no color here.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// User turn.
    User(String),
    /// Assistant turn (streamed text). `streaming` = live cursor active.
    Assistant { text: String, streaming: bool },
    /// Model reasoning (rendered muted).
    Reasoning(String),
    /// A tool is about to run. The raw `input` is KEPT (US-033): rendering
    /// derives the `Verb(target)` label from it and, eventually, the diff (EP-011); `id` pairs
    /// the call with its result.
    ToolCall {
        id: ToolCallId,
        name: String,
        input: serde_json::Value,
        input_hash: u64,
    },
    /// Result of a tool (taint + error carried for rendering). `call_id` points
    /// to the matching `ToolCall` (US-033) for the `⎿` summary.
    ToolResult {
        call_id: ToolCallId,
        content: String,
        untrusted: bool,
        is_error: bool,
        error_kind: Option<ToolErrorKind>,
    },
    /// Plan of the current task (US-009). At most one lives in the transcript:
    /// an update MOVES it to the end instead of stacking a second copy (AC4).
    Plan(agent_core::PlanView),
    /// Discreet system information (compaction, budget, ...).
    Notice(String),
    /// Error surfaced by the core.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Thinking,
}

/// Slash commands: (name, description, takes-an-argument). Single source for the
/// completion menu (rendering) AND execution (agent-cli loop). `takes_arg` =
/// the command opens a submenu / expects an argument (Enter completes instead
/// of executing). Adding one = a line here + a branch in the dispatch.
pub const COMMANDS: &[(&str, &str, bool)] = &[
    ("/help", "Show available commands", false),
    ("/models", "Choose the active model", true),
    ("/effort", "Choose the reasoning effort", true),
    (
        "/permissions",
        "Choose when Pyxis asks for confirmation",
        true,
    ),
    ("/skills", "Insert a skill into the message", true),
    ("/goal", "Set a goal and work until it is done", true),
    ("/providers", "Configure the authentication provider", true),
    ("/mcp", "Inspect MCP servers", true),
    ("/resume", "Resume a past conversation", true),
    (
        "/fork",
        "Branch at the last completed turn and continue in the branch",
        false,
    ),
    (
        "/rewind",
        "Branch at an earlier turn (/rewind <turn-id>) without touching this one",
        false,
    ),
    (
        "/approvals",
        "List the answers remembered this session (clear to forget)",
        false,
    ),
    ("/status", "Show the session configuration", false),
    ("/usage", "Show token consumption and quota", false),
    ("/hooks", "List the declared hooks", false),
    ("/diff", "Show the current workspace changes", false),
    ("/copy", "Copy the last answer to the clipboard", false),
    (
        "/init",
        "Write an AGENTS.md from a repository inspection",
        false,
    ),
    ("/compact", "Compact the context now", false),
    ("/new", "Start a new session and clear context", false),
    ("/clear", "Clear context and start fresh", false),
    ("/logout", "Sign out and delete the local credential", false),
    ("/quit", "Quit Pyxis", false),
];

/// Level 1 of `/providers`: (id, label, active). Only the subscription is
/// available for now; the API key is announced but inactive.
pub const AUTH_KINDS: &[(&str, &str, bool)] = &[
    ("subscription", "Use a subscription", true),
    ("apikey", "Use an API key", false),
];

/// Level 2 of `/providers subscription`: (id, label, active). Only Codex
/// (ChatGPT subscription) is wired; the others are announced.
pub const SUB_PROVIDERS: &[(&str, &str, bool)] = &[
    ("codex", "ChatGPT Plus/Pro (Codex Subscription)", true),
    ("anthropic", "Anthropic (Claude Pro/Max)", false),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningEffortMeta {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const REASONING_EFFORTS: &[ReasoningEffortMeta] = &[
    ReasoningEffortMeta {
        id: "none",
        label: "None",
        hint: "no reasoning",
    },
    ReasoningEffortMeta {
        id: "minimal",
        label: "Minimal",
        hint: "smallest reasoning budget",
    },
    ReasoningEffortMeta {
        id: "low",
        label: "Low",
        hint: "light reasoning",
    },
    ReasoningEffortMeta {
        id: "medium",
        label: "Medium",
        hint: "default",
    },
    ReasoningEffortMeta {
        id: "high",
        label: "High",
        hint: "deeper reasoning",
    },
    ReasoningEffortMeta {
        id: "xhigh",
        label: "Extra high",
        hint: "highest standard option",
    },
    ReasoningEffortMeta {
        id: "max",
        label: "Max",
        hint: "maximum backend effort",
    },
    ReasoningEffortMeta {
        id: "ultra",
        label: "Ultra",
        hint: "sent as max",
    },
];

pub const GPT5_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const EFFORTS_TO_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const EFFORTS_TO_ULTRA: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

/// Provider tag shown as a hint in the `/models` submenu. A single model
/// channel is wired today (ChatGPT subscription through the Codex backend).
const CODEX_TAG: &str = "[openai-codex]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMeta {
    pub slug: &'static str,
    pub tag: &'static str,
    pub default_reasoning_effort: Option<&'static str>,
    pub supported_reasoning_efforts: &'static [&'static str],
    pub incompatibility_reason: Option<&'static str>,
}

/// FALLBACK catalog, used until the backend has answered (startup,
/// offline, expired token). The authoritative list is the one the connected
/// account returns on `GET /models`: see `set_models` / `models()`. Snapshot
/// of 2026-07-24, order = backend priority.
const BUNDLED_MODELS: &[ModelMeta] = &[
    ModelMeta {
        slug: "gpt-5.6-sol",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("low"),
        supported_reasoning_efforts: EFFORTS_TO_ULTRA,
        incompatibility_reason: Some("code mode required"),
    },
    ModelMeta {
        slug: "gpt-5.6-terra",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("medium"),
        supported_reasoning_efforts: EFFORTS_TO_ULTRA,
        incompatibility_reason: Some("code mode required"),
    },
    ModelMeta {
        slug: "gpt-5.6-luna",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("medium"),
        supported_reasoning_efforts: EFFORTS_TO_MAX,
        incompatibility_reason: Some("code mode required"),
    },
    ModelMeta {
        slug: "gpt-5.5",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("medium"),
        supported_reasoning_efforts: GPT5_REASONING_EFFORTS,
        incompatibility_reason: None,
    },
    ModelMeta {
        slug: "gpt-5.4",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("medium"),
        supported_reasoning_efforts: GPT5_REASONING_EFFORTS,
        incompatibility_reason: None,
    },
    ModelMeta {
        slug: "gpt-5.4-mini",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("medium"),
        supported_reasoning_efforts: GPT5_REASONING_EFFORTS,
        incompatibility_reason: None,
    },
    ModelMeta {
        slug: "gpt-5.3-codex-spark",
        tag: CODEX_TAG,
        default_reasoning_effort: Some("high"),
        supported_reasoning_efforts: GPT5_REASONING_EFFORTS,
        incompatibility_reason: None,
    },
];

/// Catalog published by the backend for the connected account. Written once
/// per process (`set_models`), read without a lock by the rendering.
static REMOTE_MODELS: OnceLock<&'static [ModelMeta]> = OnceLock::new();

/// Model as the provider discovered it, before conversion into `ModelMeta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub slug: String,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub incompatibility_reason: Option<String>,
}

/// Active catalog: the backend one as soon as it is known, `BUNDLED_MODELS` otherwise.
pub fn models() -> &'static [ModelMeta] {
    REMOTE_MODELS.get().copied().unwrap_or(BUNDLED_MODELS)
}

/// Publishes the catalog discovered on the backend. Returns `false` when the list is
/// empty (backend that does not know our `client_version`) or when a catalog has
/// already been published: in both cases the current catalog stays in place.
///
/// The strings are deliberately leaked: the catalog is immutable and lives
/// as long as the process, which keeps `ModelMeta: Copy` and the
/// `&'static` signatures of every caller.
pub fn set_models(entries: Vec<ModelCatalogEntry>) -> bool {
    if entries.is_empty() {
        return false;
    }
    let metas: Vec<ModelMeta> = entries
        .into_iter()
        .map(|entry| ModelMeta {
            slug: String::leak(entry.slug),
            tag: CODEX_TAG,
            default_reasoning_effort: entry
                .default_reasoning_effort
                .map(|effort| &*String::leak(effort)),
            supported_reasoning_efforts: Vec::leak(
                entry
                    .supported_reasoning_efforts
                    .into_iter()
                    .map(|effort| &*String::leak(effort))
                    .collect::<Vec<&'static str>>(),
            ),
            incompatibility_reason: entry
                .incompatibility_reason
                .map(|reason| &*String::leak(reason)),
        })
        .collect();
    REMOTE_MODELS
        .set(Box::leak(metas.into_boxed_slice()))
        .is_ok()
}

pub fn reasoning_effort_label(id: &str) -> String {
    let trimmed = id.trim();
    REASONING_EFFORTS
        .iter()
        .find(|effort| effort.id.eq_ignore_ascii_case(trimmed))
        .map(|effort| effort.label.to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

pub fn normalize_reasoning_effort(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    REASONING_EFFORTS
        .iter()
        .find(|effort| effort.id == lower || effort.label.to_ascii_lowercase() == lower)
        .map(|effort| effort.id.to_string())
        .or_else(|| Some(trimmed.to_string()))
}

pub fn model_meta(model: &str) -> Option<&'static ModelMeta> {
    let trimmed = model.trim();
    models().iter().find(|meta| meta.slug == trimmed)
}

pub fn supported_reasoning_efforts_for_model(model: &str) -> &'static [&'static str] {
    let trimmed = model.trim();
    model_meta(trimmed)
        .map(|meta| meta.supported_reasoning_efforts)
        .unwrap_or(&[])
}

pub fn default_reasoning_effort_for_model(model: &str) -> Option<&'static str> {
    let trimmed = model.trim();
    model_meta(trimmed).and_then(|meta| meta.default_reasoning_effort)
}

pub fn normalize_reasoning_effort_for_model(model: &str, value: &str) -> Option<String> {
    let normalized = normalize_reasoning_effort(value)?;
    supported_reasoning_efforts_for_model(model)
        .iter()
        .any(|effort| effort.eq_ignore_ascii_case(&normalized))
        .then_some(normalized)
}

pub const DEFAULT_PERMISSION_MODE_ID: &str = "ask";
pub const QUIT_SHORTCUT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionModeMeta {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const PERMISSION_MODES: &[PermissionModeMeta] = &[
    PermissionModeMeta {
        id: "ask",
        label: "Ask for approval",
        hint: "ask before sensitive actions",
    },
    PermissionModeMeta {
        id: "accept-edits",
        label: "Auto-approve edits",
        hint: "auto-approve write/edit, ask for sensitive actions",
    },
    PermissionModeMeta {
        id: "auto",
        label: "Approve for me",
        hint: "do not interrupt except after recent taint",
    },
    PermissionModeMeta {
        id: "full-access",
        label: "Full Access",
        hint: "bypass confirmations, sandbox unchanged",
    },
    PermissionModeMeta {
        id: "read-only",
        label: "Read Only",
        hint: "strict read-only mode",
    },
];

pub fn permission_mode_meta(id: &str) -> Option<&'static PermissionModeMeta> {
    PERMISSION_MODES.iter().find(|mode| mode.id == id)
}

pub fn permission_mode_label(id: &str) -> &'static str {
    permission_mode_meta(id)
        .map(|mode| mode.label)
        .unwrap_or("Ask for approval")
}

/// Is the text a real Pyxis command? (1st word in COMMANDS). A message
/// starting with a `/<skill>` is NOT one -> it goes to the agent.
/// Byte offset, in `s`, of the grapheme boundary reaching at most `col`
/// terminal columns. Used to keep the column during vertical navigation without
/// ever landing in the middle of a character (US-009 AC5).
fn offset_at_width(s: &str, col: usize) -> usize {
    let mut used = 0usize;
    for (i, g) in s.grapheme_indices(true) {
        let w = measure::width(g);
        if used + w > col {
            return i;
        }
        used += w;
    }
    s.len()
}

fn is_command(text: &str) -> bool {
    // A Pyxis command fits on one line: a multi-line message starting
    // with `/resume ...` is a prompt, not a command (US-009).
    if text.contains('\n') {
        return false;
    }
    let first = text.split(' ').next().unwrap_or("");
    COMMANDS.iter().any(|(name, _, _)| *name == first)
}

/// Does the `name` command expect an argument / a submenu?
fn command_takes_arg(name: &str) -> bool {
    COMMANDS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, takes)| *takes)
        .unwrap_or(false)
}

/// A completion menu item (unified source: commands, models, sessions,
/// providers). `id` = value passed to the action; `label`/`hint` = display;
/// `enabled` = selectable ("coming soon" items are greyed out).
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: String,
    pub label: String,
    pub hint: String,
    pub enabled: bool,
}

impl MenuItem {
    fn new(id: &str, label: &str, hint: &str, enabled: bool) -> Self {
        Self {
            id: id.to_string(),
            label: terminal_safe_text(label),
            hint: terminal_safe_text(hint),
            enabled,
        }
    }
}

fn terminal_safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Which submenu does the current input open? (breadcrumb in the input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    None,
    Commands,
    Models,
    Effort,
    Resume,
    Skills,
    Files,
    Permissions,
    ProviderAuth,
    ProviderList,
    /// Level 3: actions on a provider (connect/disconnect).
    ProviderActions,
    /// `/mcp `: list of MCP servers (status badge).
    McpList,
    /// `/mcp <server> `: actions on a server (connect/disconnect/tools).
    McpActions,
}

/// Entry of the `/resume` submenu (filled by agent-cli from the disk).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// Identifier resolved on the CLI side (`<id>.jsonl` file name).
    pub id: String,
    /// Displayed label: summary of the conversation (1st message).
    pub label: String,
    /// Secondary hint displayed muted (e.g. "12 msgs - 2 h ago").
    pub hint: String,
}

/// Connection status of an MCP server (`/mcp` submenu). Mirrors the
/// `agent_mcp::McpServer` enum on the display side; agent-cli does the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

/// Entry of the `/mcp` submenu (filled by agent-cli from the MCP registry).
#[derive(Debug, Clone)]
pub struct McpServerMeta {
    pub name: String,
    pub status: McpStatus,
    pub source: String,
    pub needs_trust: bool,
    /// Number of exposed tools (meaningful only when `Connected`).
    pub tool_count: usize,
    /// Remote (Streamable HTTP) server: only those have an OAuth endpoint, so
    /// only those are offered a login.
    pub remote: bool,
}

/// Rebuilds the displayable transcript from canonical messages (resuming
/// a session). Rough inverse of `AppState::apply`: System ignored,
/// thinking -> reasoning, tool_use -> tool call, tool_result -> result.
pub fn blocks_from_messages(messages: &[Message]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {}
            Role::User => {
                let t = m.text();
                if !t.is_empty() {
                    blocks.push(Block::User(t));
                }
            }
            Role::Assistant => {
                for b in &m.content {
                    if let ContentBlock::Thinking { text } = b {
                        blocks.push(Block::Reasoning(text.clone()));
                    }
                }
                let text = m.text();
                if !text.is_empty() {
                    blocks.push(Block::Assistant {
                        text,
                        streaming: false,
                    });
                }
                for b in &m.content {
                    if let ContentBlock::ToolUse {
                        id, name, input, ..
                    } = b
                    {
                        blocks.push(Block::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            input_hash: crate::cache::value_hash(input),
                        });
                    }
                }
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        untrusted,
                        is_error,
                        error_kind,
                        ..
                    } = b
                    {
                        blocks.push(Block::ToolResult {
                            call_id: tool_use_id.clone(),
                            content: content.clone(),
                            untrusted: *untrusted,
                            is_error: *is_error,
                            error_kind: *error_kind,
                        });
                    }
                }
            }
        }
    }
    blocks
}

/// Extracts the prompt history (user messages, oldest -> most recent) of a
/// resumed session, for arrow-key navigation.
pub fn prompts_from_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(Message::text)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

/// Confirmation request presented to the user (generic: the agent-cli loop
/// builds it from the `PermissionRequest` of `agent-tools`, pre-rendering
/// the preview through `diff`: a real diff for `edit`/`write`, context lines
/// for bash/unknown, SHARED with the inline transcript diff (US-039).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionPrompt {
    pub title: String,
    pub reason: String,
    pub preview: crate::diff::Diff,
    pub call_id: Option<ToolCallId>,
    pub mode: Option<String>,
    pub taint_forced: bool,
    /// The answer can be remembered for the session (US-009 AC1): the dialog
    /// then offers the session options.
    pub memoizable: bool,
    /// Why remembering is unavailable, when there is a reason worth showing
    /// (US-009 AC2).
    pub memo_note: Option<String>,
}

impl PermissionPrompt {
    pub fn new(
        title: impl Into<String>,
        reason: impl Into<String>,
        preview: crate::diff::Diff,
    ) -> Self {
        Self {
            title: title.into(),
            reason: reason.into(),
            preview,
            call_id: None,
            mode: None,
            taint_forced: false,
            memoizable: false,
            memo_note: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub blocks: Vec<Block>,
    pub input: String,
    /// Cursor position in the input, as a valid UTF-8 byte offset.
    /// Moves/deletions follow graphemes; rendering converts this offset
    /// into terminal width through `unicode-width`.
    pub cursor: usize,
    pub status: Status,
    pub pending: Option<PermissionPrompt>,
    pub truecolor: bool,
    /// Scroll offset UPWARD (0 = pinned at the bottom, follows the live output).
    pub scroll: usize,
    /// Max scroll bound, recomputed on every frame by the rendering (lines AFTER
    /// wrapping minus visible height). Rendering -> input feedback cache: lets us clamp
    /// the scroll without duplicating the wrap computation outside of `render`.
    pub scroll_max: Cell<usize>,
    /// Cache of styled lines per block (US-041): rebuild only the streaming
    /// block, serve the others from the cache. Interior mutability (same pattern
    /// as `scroll_max`) so that `render` stays pure (`&AppState` signature).
    pub(crate) render_cache: RefCell<crate::cache::RenderCache>,
    pub model: String,
    /// Workspace name (current directory) shown in the status line; empty = hidden.
    pub workspace: String,
    /// Durable identity of the conversation, as the runtime reports it
    /// (US-017 AC5). Held HERE so the frontend answers "which thread, which
    /// turn, in which state, with how many inputs waiting?" from its own state
    /// and never by re-reading the store.
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// `queued`, `running`, `needs_input`, `completed`, `interrupted` or
    /// `failed`. `None` before the first turn of the thread.
    pub turn_state: Option<String>,
    /// Inputs accepted and not consumed yet: queued turns plus the steering
    /// inputs of the running turn.
    pub pending_inputs: usize,
    /// Fraction of context consumed (0-100). `None` = unknown -> segment hidden.
    /// Fed by `AgentEvent::ModelTurn` (US-004), and only when the backend
    /// reported a usage AND the window of the active model is known: an
    /// estimated fill is worse than no fill.
    pub context_pct: Option<u8>,
    /// Context occupied at the last round-trip, as measured by the backend
    /// (US-004). `None` = not reported.
    pub context_tokens: Option<u32>,
    /// Context window of the active model when the backend declares one
    /// (US-001). `None` = unknown.
    pub context_window: Option<u32>,
    /// Tokens cumulated since the start of the session, as carried by
    /// `ModelTurn` (real when the backend reports them, estimated otherwise).
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Subscription quota state (US-003). `None` as long as the backend has
    /// served nothing: nothing is then displayed.
    pub quota: Option<agent_core::quota::QuotaSnapshot>,
    /// Reasoning effort displayed with the model in the footer.
    pub reasoning_effort: Option<String>,
    /// Permission mode displayed in the footer and the `/permissions` submenu.
    permission_mode: String,
    /// Selected index in the slash command menu (0 = first line).
    pub completion_index: usize,
    /// Resumable sessions (`/resume` submenu), filled by agent-cli.
    pub sessions: Vec<SessionMeta>,
    /// Available skills (`~/.agents/skills`), `/skills` submenu. Read before the
    /// sandbox (directory outside the workspace) and injected by agent-cli.
    pub skills: Vec<String>,
    /// Files mentionable through `@`, bounded and provided by agent-cli.
    pub files: Vec<String>,
    /// Connected to the active provider (status line badge + providers submenu).
    pub provider_connected: bool,
    /// Known MCP servers + status (`/mcp` submenu), filled by agent-cli.
    pub mcp_servers: Vec<McpServerMeta>,
    /// History of submitted prompts (oldest -> most recent), navigable with arrows.
    pub history: Vec<String>,
    /// Position in the history: `None` = current draft, `Some(i)` = on
    /// `history[i]`. The draft is saved in `draft` on the first Up.
    history_pos: Option<usize>,
    draft: String,
    pub should_quit: bool,
    shutdown_in_progress: bool,
    quit_shortcut_expires_at: Option<Instant>,
    /// Shortcut cheatsheet toggled with `?` on an empty composer. The ONLY
    /// footer state that is stored: the other modes are derived from the input
    /// and from the quit timer, so they cannot drift out of sync.
    shortcut_overlay_open: bool,
    // ── Live progress (EP-013) ──────────────────────────────────────────────────
    /// Spinner animation tick, advanced by the loop (~10 fps) as long as a turn
    /// is active. Rendering picks the frame from this counter (stays pure).
    pub spinner_tick: usize,
    /// Elapsed time of the current turn (`None` outside a turn); fed by the loop
    /// (which owns the clock): `render` never reads the time.
    pub turn_elapsed: Option<Duration>,
    /// Cumulated characters (text + reasoning) of the current turn. Bookkeeping
    /// for `stream_start`, which rewinds the counter when the core abandons a
    /// stream; it is NOT a consumption measure and feeds no display (US-004:
    /// the indicator comes from the backend counters only). On a `/goal` loop it
    /// cumulates every re-prompt: reset only on the rising edge of `running`
    /// (`begin_turn`).
    pub turn_chars: usize,
    /// Reduced motion (`NO_COLOR` / `PYXIS_REDUCED_MOTION`): spinner degraded to a pulsing dot.
    pub reduced_motion: bool,
    /// New blocks that arrived while the user had scrolled up the transcript
    /// ("back to bottom" pill, US-046). Reset to 0 as soon as the bottom is reached.
    pub unseen: usize,
    /// Full transcript overlay, opened with Ctrl+T. Its scroll is separate from
    /// the main thread scroll, to come back exactly where the user was.
    transcript_overlay_open: bool,
    transcript_overlay_scroll: usize,
    transcript_overlay_scroll_max: Cell<usize>,
    transcript_overlay_page_height: Cell<usize>,
    /// Start of the current live stream: block index and character counter.
    /// Used to drop abandoned deltas when the core retries/recovers.
    stream_start: Option<(usize, usize)>,
    /// Large pastes replaced by a summary in `input` (US-011). The
    /// full content is re-expanded at submission time.
    pastes: Vec<PendingPaste>,
    /// Output of the running tool, streamed before its result
    /// (US-015). Cleared when the result arrives, except on interruption: what the
    /// command had already produced then stays visible.
    pub live_output: Option<LiveOutput>,
}

/// Partial output of a tool call still in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveOutput {
    pub call_id: ToolCallId,
    pub text: String,
}

/// Bounds of the live display (US-015 AC3): the visible output stays short, the
/// truncation policy of the final result is unchanged.
pub const LIVE_OUTPUT_MAX_LINES: usize = 8;
const LIVE_OUTPUT_MAX_BYTES: usize = 8_192;

/// A summarized paste: what is displayed, and what will really be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingPaste {
    placeholder: String,
    content: String,
}

/// Past this line count, a paste is summarized in the composer rather
/// than inserted as is (US-011 AC2).
pub const PASTE_SUMMARY_MIN_LINES: usize = 500;

/// Action derived from a key, interpreted by the agent-cli loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    None,
    Submit(String),
    /// Slash command to run (full line, args included: `/model gpt-5.5`).
    Command(String),
    Interrupt,
    Quit,
    /// Answer to the pending confirmation: what to do now, and whether to
    /// remember it for the session (US-009). `remember` is only ever true when
    /// the prompt declared itself memoizable.
    Permission {
        allow: bool,
        remember: bool,
    },
    ScrollUp,
    ScrollDown,
}

/// Modifiers that turn Enter into a newline insertion. Shift is among
/// them: terminals that report modifiers on Enter make
/// Shift+Enter equivalent to Alt+Enter (US-009 AC2). The others emit no
/// modifier, and Enter submits as before (AC3).
const NEWLINE_MODIFIERS: KeyModifiers = KeyModifiers::ALT.union(KeyModifiers::SHIFT);

fn is_ctrl_key(key: &KeyEvent, expected: char) -> bool {
    matches!(
        key.code,
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && c.eq_ignore_ascii_case(&expected)
    )
}

fn is_plain_char_key(key: &KeyEvent, expected: char) -> bool {
    matches!(
        key.code,
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && c.eq_ignore_ascii_case(&expected)
    )
}

impl AppState {
    pub fn new(model: impl Into<String>, truecolor: bool) -> Self {
        Self {
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: Status::Idle,
            pending: None,
            truecolor,
            scroll: 0,
            scroll_max: Cell::new(0),
            render_cache: RefCell::new(crate::cache::RenderCache::default()),
            model: model.into(),
            workspace: String::new(),
            thread_id: String::new(),
            turn_id: None,
            turn_state: None,
            pending_inputs: 0,
            context_pct: None,
            context_tokens: None,
            context_window: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            quota: None,
            reasoning_effort: None,
            permission_mode: DEFAULT_PERMISSION_MODE_ID.to_string(),
            completion_index: 0,
            sessions: Vec::new(),
            skills: Vec::new(),
            files: Vec::new(),
            provider_connected: false,
            mcp_servers: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            should_quit: false,
            shutdown_in_progress: false,
            quit_shortcut_expires_at: None,
            shortcut_overlay_open: false,
            spinner_tick: 0,
            turn_elapsed: None,
            turn_chars: 0,
            reduced_motion: false,
            unseen: 0,
            transcript_overlay_open: false,
            transcript_overlay_scroll: 0,
            transcript_overlay_scroll_max: Cell::new(0),
            transcript_overlay_page_height: Cell::new(10),
            stream_start: None,
            pastes: Vec::new(),
            live_output: None,
        }
    }

    // ── Input editing with a positionable cursor ───────────────────────────────

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.input.len());
        while self.cursor > 0 && !self.input.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    fn prev_grapheme_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(idx, _)| idx)
    }

    fn next_grapheme_boundary(&self) -> Option<usize> {
        self.input[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| self.cursor + idx)
            .or_else(|| (self.cursor < self.input.len()).then_some(self.input.len()))
    }

    /// Replaces the input and puts the cursor at the end (recall, completion, insertion).
    pub fn set_input(&mut self, value: String) {
        self.cursor = value.len();
        self.input = value;
    }

    pub fn permission_mode_id(&self) -> &str {
        &self.permission_mode
    }

    pub fn permission_mode_label(&self) -> &'static str {
        permission_mode_label(&self.permission_mode)
    }

    pub fn set_permission_mode(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.permission_mode = if permission_mode_meta(&id).is_some() {
            id
        } else {
            DEFAULT_PERMISSION_MODE_ID.to_string()
        };
    }

    pub fn quit_shortcut_hint_visible(&self) -> bool {
        self.quit_shortcut_expires_at
            .is_some_and(|expires_at| Instant::now() < expires_at)
    }

    pub fn quit_shortcut_remaining(&self) -> Option<Duration> {
        self.quit_shortcut_expires_at
            .and_then(|expires_at| expires_at.checked_duration_since(Instant::now()))
    }

    pub fn clear_quit_shortcut_hint(&mut self) {
        self.quit_shortcut_expires_at = None;
    }

    pub fn shortcut_overlay_open(&self) -> bool {
        self.shortcut_overlay_open
    }

    /// Effective footer mode, resolved as a priority waterfall (Codex
    /// `ChatComposer::footer_mode`): a transient instruction always outranks the
    /// ambient status line, and the base mode only says whether a draft exists.
    pub fn footer_mode(&self) -> FooterMode {
        if self.shortcut_overlay_open {
            return FooterMode::ShortcutOverlay;
        }
        if self.quit_shortcut_hint_visible() {
            return FooterMode::QuitShortcutReminder;
        }
        if self.input.is_empty() {
            FooterMode::ComposerEmpty
        } else {
            FooterMode::ComposerHasDraft
        }
    }

    /// Handles the overlay toggle key. Returns `true` when the key was consumed.
    ///
    /// The toggle only fires on an EMPTY composer, so typing or pasting a `?`
    /// still inserts the character instead of opening help.
    fn on_shortcut_overlay_key(&mut self, key: &KeyEvent) -> bool {
        let toggles = matches!(key.code, KeyCode::Char('?'))
            && (key.modifiers - KeyModifiers::SHIFT).is_empty()
            && self.input.is_empty()
            && !self.shutdown_in_progress;
        if toggles {
            self.shortcut_overlay_open = !self.shortcut_overlay_open;
            return true;
        }
        if !self.shortcut_overlay_open {
            return false;
        }
        // Any other key closes the overlay; Esc does nothing else, so it does
        // not also interrupt the running turn.
        self.shortcut_overlay_open = false;
        key.code == KeyCode::Esc
    }

    pub fn shutdown_in_progress(&self) -> bool {
        self.shutdown_in_progress
    }

    pub fn show_shutdown_in_progress(&mut self) {
        self.shutdown_in_progress = true;
        self.pending = None;
        self.status = Status::Idle;
        self.completion_index = 0;
        self.shortcut_overlay_open = false;
        self.clear_quit_shortcut_hint();
    }

    fn arm_quit_shortcut(&mut self) {
        self.quit_shortcut_expires_at = Instant::now()
            .checked_add(QUIT_SHORTCUT_TIMEOUT)
            .or_else(|| Some(Instant::now()));
    }

    fn quit_shortcut_active(&self) -> bool {
        self.quit_shortcut_hint_visible()
    }

    fn on_ctrl_c(&mut self) -> InputAction {
        if self.quit_shortcut_active() {
            self.clear_quit_shortcut_hint();
            self.should_quit = true;
            return InputAction::Quit;
        }

        self.arm_quit_shortcut();
        if self.status == Status::Thinking {
            InputAction::Interrupt
        } else {
            InputAction::None
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.pastes.clear();
    }

    /// Inserts a char at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Inserts a string at the cursor position (the cursor follows it).
    pub fn insert_str(&mut self, s: &str) {
        self.clamp_cursor();
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Deletes the char BEFORE the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.clamp_cursor();
        if let Some(start) = self.prev_grapheme_boundary() {
            self.input.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// Deletes the char UNDER the cursor (Delete).
    pub fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.next_grapheme_boundary().unwrap_or(self.input.len());
        self.input.replace_range(self.cursor..end, "");
    }

    fn move_left(&mut self) {
        self.clamp_cursor();
        if let Some(prev) = self.prev_grapheme_boundary() {
            self.cursor = prev;
        }
    }
    fn move_right(&mut self) {
        self.clamp_cursor();
        if let Some(next) = self.next_grapheme_boundary() {
            self.cursor = next;
        }
    }
    /// Start / end of the LOGICAL line containing `at` (bounds as byte offsets).
    fn line_bounds(&self, at: usize) -> (usize, usize) {
        let start = self.input[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = self.input[at..]
            .find('\n')
            .map_or(self.input.len(), |i| at + i);
        (start, end)
    }

    /// Home / Ctrl+A: start of the current line (identical to offset 0 as long
    /// as the input fits on one line, hence unchanged behavior).
    fn move_home(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_bounds(self.cursor).0;
    }
    fn move_end(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_bounds(self.cursor).1;
    }

    /// Moves up one logical line while keeping the displayed column. Returns
    /// `false` when the cursor is already on the first line: the caller then
    /// recalls the history (US-009 AC4).
    fn move_line_up(&mut self) -> bool {
        self.clamp_cursor();
        let (start, _) = self.line_bounds(self.cursor);
        if start == 0 {
            return false;
        }
        let col = measure::width(&self.input[start..self.cursor]);
        let prev_end = start - 1;
        let (prev_start, _) = self.line_bounds(prev_end);
        self.cursor = prev_start + offset_at_width(&self.input[prev_start..prev_end], col);
        true
    }

    /// Moves down one logical line. `false` = already on the last line.
    fn move_line_down(&mut self) -> bool {
        self.clamp_cursor();
        let (start, end) = self.line_bounds(self.cursor);
        if end >= self.input.len() {
            return false;
        }
        let col = measure::width(&self.input[start..self.cursor]);
        let next_start = end + 1;
        let (_, next_end) = self.line_bounds(next_start);
        self.cursor = next_start + offset_at_width(&self.input[next_start..next_end], col);
        true
    }

    /// Inserts a newline without submitting (Alt+Enter, Ctrl+J, Shift+Enter).
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Inserts pasted content: stripped of control sequences, and summarized
    /// past `PASTE_SUMMARY_MIN_LINES` lines (US-011).
    pub fn insert_paste(&mut self, raw: &str) {
        let text = crate::composer::sanitize_paste(raw);
        let lines = text.lines().count();
        if lines <= PASTE_SUMMARY_MIN_LINES {
            self.insert_str(&text);
            return;
        }
        let placeholder = format!("[collage : {lines} lignes]");
        self.insert_str(&placeholder);
        self.pastes.push(PendingPaste {
            placeholder,
            content: text,
        });
    }

    /// Re-expands the summarized pastes: it is the FULL content that goes to
    /// the model, never the displayed summary (US-011 AC3).
    ///
    /// Matching by text, in order of appearance, each paste being
    /// consumed only once. Accepted limitation: two pastes of the same size, one of
    /// which was deleted by hand, can be swapped.
    fn expand_pastes(&self, text: &str) -> String {
        if self.pastes.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut used = vec![false; self.pastes.len()];
        loop {
            let next = self
                .pastes
                .iter()
                .enumerate()
                .filter(|(i, _)| !used[*i])
                .filter_map(|(i, p)| rest.find(&p.placeholder).map(|at| (at, i)))
                .min_by_key(|(at, i)| (*at, *i));
            let Some((at, i)) = next else {
                break;
            };
            out.push_str(&rest[..at]);
            out.push_str(&self.pastes[i].content);
            rest = &rest[at + self.pastes[i].placeholder.len()..];
            used[i] = true;
        }
        out.push_str(rest);
        out
    }

    fn delete_prev_word(&mut self) {
        self.clamp_cursor();
        while self.cursor > 0 {
            let Some(prev) = self.prev_grapheme_boundary() else {
                break;
            };
            if !self.input[prev..self.cursor].trim().is_empty() {
                break;
            }
            self.input.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
        while self.cursor > 0 {
            let Some(prev) = self.prev_grapheme_boundary() else {
                break;
            };
            if self.input[prev..self.cursor].trim().is_empty() {
                break;
            }
            self.input.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    /// Files an `AgentEvent` from the core into the transcript.
    pub fn apply(&mut self, ev: &AgentEvent) {
        let before = self.blocks.len();
        match ev {
            AgentEvent::StreamReset => self.reset_streaming(),
            AgentEvent::Text(t) => {
                self.begin_streaming();
                self.status = Status::Thinking;
                self.turn_chars += t.chars().count();
                match self.blocks.last_mut() {
                    Some(Block::Assistant {
                        text,
                        streaming: true,
                    }) => text.push_str(t),
                    _ => self.blocks.push(Block::Assistant {
                        text: t.clone(),
                        streaming: true,
                    }),
                }
            }
            AgentEvent::Reasoning(t) => {
                self.begin_streaming();
                self.status = Status::Thinking;
                self.turn_chars += t.chars().count();
                match self.blocks.last_mut() {
                    Some(Block::Reasoning(r)) => r.push_str(t),
                    _ => self.blocks.push(Block::Reasoning(t.clone())),
                }
            }
            AgentEvent::ReasoningReplayDisabled { reason } => {
                self.blocks.push(Block::Notice(format!(
                    "reasoning replay disabled: {reason}"
                )));
            }
            AgentEvent::RetryScheduled(view) => {
                self.blocks.push(Block::Notice(format!(
                    "retry {}/{} in {} ms ({:?})",
                    view.ordinal, view.max_attempts, view.delay_ms, view.cause
                )));
            }
            AgentEvent::CredentialRefresh(view) => {
                self.blocks.push(Block::Notice(format!(
                    "credential refresh: {:?}",
                    view.outcome
                )));
            }
            AgentEvent::ToolCall(view) => {
                self.finalize_streaming();
                self.live_output = None;
                self.blocks.push(Block::ToolCall {
                    id: view.id.clone(),
                    name: view.name.clone(),
                    input: view.input.clone(),
                    input_hash: crate::cache::value_hash(&view.input),
                });
            }
            AgentEvent::ToolOutputDelta(view) => {
                self.push_live_output(&view.id, &view.chunk);
            }
            AgentEvent::ToolResult(view) => {
                // AC4: on interruption, the output already produced stays displayed;
                // the synthetic result does not contain it. Otherwise, the final
                // result replaces the live preview.
                if self
                    .live_output
                    .as_ref()
                    .is_some_and(|live| live.call_id == view.id)
                    && view.content != agent_core::INTERRUPTED_TOOL_RESULT
                {
                    self.live_output = None;
                }
                // Defensive symmetry with ToolCall: should an orphan result arrive
                // without a preceding call, an Assistant{streaming} left open must not
                // keep a phantom live cursor.
                self.finalize_streaming();
                self.blocks.push(Block::ToolResult {
                    call_id: view.id.clone(),
                    content: view.content.clone(),
                    untrusted: view.untrusted,
                    is_error: view.is_error,
                    error_kind: view.error_kind,
                });
            }
            AgentEvent::Plan(view) => {
                self.finalize_streaming();
                // AC4: the previous plan leaves the transcript, so the reader
                // sees ONE plan, in its current state, at the point of the
                // conversation where it was last updated.
                self.blocks.retain(|b| !matches!(b, Block::Plan(_)));
                self.blocks.push(Block::Plan(view.clone()));
            }
            AgentEvent::Compacted(_) => self.blocks.push(Block::Notice("context compacted".into())),
            // Turn accounting (US-017, US-004): no block in the transcript, but
            // this IS the source of the consumption indicator.
            AgentEvent::ModelTurn(view) => self.observe_model_turn(view),
            AgentEvent::Quota(snapshot) => self.quota = Some(*snapshot),
            AgentEvent::TurnDiff(view) => self.blocks.push(Block::Notice(turn_diff_summary(view))),
            AgentEvent::PermissionAsk(req) => self
                .blocks
                .push(Block::Notice(format!("permission: {}", req.tool))),
            AgentEvent::EndTurn => {
                self.finalize_streaming();
                self.status = Status::Idle;
            }
            AgentEvent::Interrupted => {
                self.finalize_streaming();
                self.pending = None;
                self.blocks.push(Block::Notice("interrupted".into()));
                self.status = Status::Idle;
            }
            AgentEvent::Exhausted(reason) => {
                self.finalize_streaming();
                self.blocks
                    .push(Block::Notice(format!("stopped: {reason:?}")));
                self.status = Status::Idle;
            }
            AgentEvent::Error(e) => {
                self.finalize_streaming();
                self.blocks.push(Block::Error(e.to_string()));
                self.status = Status::Idle;
            }
        }
        // "New message" pill (US-046): when the user has scrolled up the
        // transcript, report the content that appeared out of their view.
        if self.scroll > 0 {
            if self.blocks.len() > before {
                self.unseen += self.blocks.len() - before;
            } else if matches!(ev, AgentEvent::Text(_) | AgentEvent::Reasoning(_)) {
                // Stream that APPENDS to the last block (no new block): report at
                // least "content arrived" without inflating the counter per token.
                self.unseen = self.unseen.max(1);
            }
        }
    }

    /// US-004: the consumption indicator is fed by the backend counters and by
    /// the real window of the model, never by a local estimate. A round-trip
    /// without a reported usage leaves the last known measure in place (the
    /// context did not shrink) rather than displaying a fabricated value; as
    /// long as no usage has ever arrived, the indicator stays absent.
    fn observe_model_turn(&mut self, view: &agent_core::ModelTurnView) {
        self.total_input_tokens = view.input_tokens;
        self.total_output_tokens = view.output_tokens;
        if let Some(window) = view.context_window {
            self.context_window = Some(window);
        }
        if let Some(tokens) = view.context_tokens {
            self.context_tokens = Some(tokens);
        }
        if let (Some(tokens), Some(window)) = (self.context_tokens, self.context_window)
            && window > 0
        {
            let pct = (u64::from(tokens) * 100).div_ceil(u64::from(window));
            self.context_pct = Some(pct.min(100) as u8);
        }
    }

    /// Pushes the user turn (called on submission) and records it in the
    /// navigable history (consecutive dedup, `ignoredups` style).
    pub fn push_user(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
        }
        self.history_pos = None;
        self.draft.clear();
        self.blocks.push(Block::User(text));
        self.live_output = None;
        self.status = Status::Thinking;
        self.scroll = 0;
        self.unseen = 0;
    }

    /// Accumulates an output fragment of the running tool, bounded in bytes then in
    /// lines: a chatty `cargo build` does not push the transcript off screen.
    fn push_live_output(&mut self, call_id: &ToolCallId, chunk: &str) {
        let live = match &mut self.live_output {
            Some(live) if live.call_id == *call_id => live,
            _ => {
                self.live_output = Some(LiveOutput {
                    call_id: call_id.clone(),
                    text: String::new(),
                });
                match &mut self.live_output {
                    Some(live) => live,
                    None => return,
                }
            }
        };
        live.text.push_str(chunk);
        if live.text.len() > LIVE_OUTPUT_MAX_BYTES {
            let mut cut = live.text.len() - LIVE_OUTPUT_MAX_BYTES;
            while cut < live.text.len() && !live.text.is_char_boundary(cut) {
                cut += 1;
            }
            live.text.drain(..cut);
        }
        let lines = live.text.lines().count();
        if lines > LIVE_OUTPUT_MAX_LINES {
            let skip = lines - LIVE_OUTPUT_MAX_LINES;
            let kept: Vec<&str> = live.text.lines().skip(skip).collect();
            live.text = kept.join("\n");
        }
    }

    /// Live output lines to display under the running tool (at most
    /// `LIVE_OUTPUT_MAX_LINES`, without any ANSI sequence).
    pub fn live_output_lines(&self) -> Vec<String> {
        self.live_output
            .as_ref()
            .map(|live| {
                live.text
                    .lines()
                    .rev()
                    .take(LIVE_OUTPUT_MAX_LINES)
                    .map(crate::render::sanitize)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replaces the navigable history (resuming a session) and resets the
    /// navigation.
    pub fn load_history(&mut self, prompts: Vec<String>) {
        self.history = prompts;
        self.history_pos = None;
        self.draft.clear();
    }

    /// Up arrow: goes back to an older prompt. Saves the draft on the
    /// first press; stops on the oldest (no wrap).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_pos = Some(pos);
        let v = self.history[pos].clone();
        self.set_input(v);
        self.completion_index = 0;
    }

    /// Down arrow: goes forward to a more recent prompt; past the most recent,
    /// restores the draft.
    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_pos = Some(i + 1);
                let v = self.history[i + 1].clone();
                self.set_input(v);
                self.completion_index = 0;
            }
            Some(_) => {
                self.history_pos = None;
                let d = std::mem::take(&mut self.draft);
                self.set_input(d);
                self.completion_index = 0;
            }
        }
    }

    fn finalize_streaming(&mut self) {
        if let Some(Block::Assistant { streaming, .. }) = self.blocks.last_mut() {
            *streaming = false;
        }
        self.stream_start = None;
    }

    fn begin_streaming(&mut self) {
        if self.stream_start.is_none() {
            self.stream_start = Some((self.blocks.len(), self.turn_chars));
        }
    }

    fn reset_streaming(&mut self) {
        if let Some((block_start, chars_start)) = self.stream_start.take() {
            self.blocks.truncate(block_start);
            self.turn_chars = chars_start;
        }
        self.status = Status::Thinking;
    }

    /// Scrolls up the transcript by `n` lines, clamped to the bound computed at the
    /// last render (`scroll_max`): no over-scroll past the beginning.
    pub fn scroll_up(&mut self, n: u16) {
        // Leaving the bottom starts from a blank counter: any residual `unseen` (e.g. a
        // block pushed while we were already pinned at the bottom) is dropped; we only
        // count the content arriving AFTER this scroll (US-046).
        if self.scroll == 0 {
            self.unseen = 0;
        }
        self.scroll = self
            .scroll
            .saturating_add(n as usize)
            .min(self.scroll_max.get());
    }

    /// Scrolls back down by `n` lines (0 = pinned at the bottom, follows the live output).
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n as usize);
        // Back at the bottom -> auto-follow resumes, no more "new messages" (US-046).
        if self.scroll == 0 {
            self.unseen = 0;
        }
    }

    pub fn transcript_overlay_open(&self) -> bool {
        self.transcript_overlay_open
    }

    pub fn transcript_overlay_scroll(&self) -> usize {
        self.transcript_overlay_scroll
    }

    pub fn open_transcript_overlay(&mut self) {
        self.transcript_overlay_open = true;
        self.transcript_overlay_scroll = 0;
    }

    pub fn close_transcript_overlay(&mut self) {
        self.transcript_overlay_open = false;
    }

    pub fn set_transcript_overlay_metrics(&self, max_scroll: usize, page_height: u16) {
        self.transcript_overlay_scroll_max.set(max_scroll);
        self.transcript_overlay_page_height
            .set((page_height as usize).max(1));
    }

    fn transcript_overlay_scroll_up(&mut self, n: usize) {
        self.transcript_overlay_scroll = self
            .transcript_overlay_scroll
            .saturating_add(n)
            .min(self.transcript_overlay_scroll_max.get());
    }

    fn transcript_overlay_scroll_down(&mut self, n: usize) {
        self.transcript_overlay_scroll = self.transcript_overlay_scroll.saturating_sub(n);
    }

    fn transcript_overlay_page_height(&self) -> usize {
        self.transcript_overlay_page_height.get().max(1)
    }

    fn jump_transcript_overlay_top(&mut self) {
        self.transcript_overlay_scroll = self.transcript_overlay_scroll_max.get();
    }

    fn jump_transcript_overlay_bottom(&mut self) {
        self.transcript_overlay_scroll = 0;
    }

    /// Number of blocks rebuilt at the last render (US-041 instrumentation): 0 =
    /// everything served from the cache. Exposed for the cache performance tests.
    pub fn render_rebuilds(&self) -> usize {
        self.render_cache.borrow().rebuilds()
    }

    /// Starts tracking the progress of a turn (rising edge of `running` on the
    /// loop side, US-044/045): resets spinner, duration and token counter.
    pub fn begin_turn(&mut self) {
        self.spinner_tick = 0;
        self.turn_elapsed = None;
        self.turn_chars = 0;
    }

    /// Advances the animation and updates the elapsed time (called by the loop
    /// tick as long as a turn is active, US-044/045). `render` stays pure: it never
    /// reads the clock, it consumes these values.
    pub fn tick_progress(&mut self, elapsed: Duration) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        self.turn_elapsed = Some(elapsed);
    }

    /// End of turn (falling edge of `running`): the indicators disappear
    /// cleanly, without a counter that keeps running (US-045).
    pub fn end_turn(&mut self) {
        self.turn_elapsed = None;
    }

    /// Which submenu does the input open? (breadcrumb in the input:
    /// `/providers subscription ...` = level 2, `/providers ...` = level 1, etc.)
    fn menu_kind(&self) -> Menu {
        let i = self.input.as_str();
        // A multi-line input is never a command: without this guardrail,
        // a paste starting with `/resume ` would open a menu that would capture
        // Enter instead of submitting (US-009).
        if i.contains('\n') {
            return Menu::None;
        }
        if let Some(rest) = i.strip_prefix("/providers ") {
            if let Some(rest2) = rest.strip_prefix("subscription ") {
                // "<provider>" followed by a space -> level 3 (provider actions).
                let prov = rest2.split(' ').next().unwrap_or("");
                if !prov.is_empty()
                    && rest2.len() > prov.len()
                    && SUB_PROVIDERS.iter().any(|(id, _, _)| *id == prov)
                {
                    Menu::ProviderActions
                } else {
                    Menu::ProviderList
                }
            } else {
                Menu::ProviderAuth
            }
        } else if i.strip_prefix("/mcp ").is_some() {
            // McpActions as soon as a known server is fully typed (followed by a
            // space); otherwise we keep filtering the list. `active_mcp_server` handles
            // names containing spaces.
            if self.active_mcp_server().is_empty() {
                Menu::McpList
            } else {
                Menu::McpActions
            }
        } else if i.starts_with("/resume ") {
            Menu::Resume
        } else if i.starts_with("/models ") {
            Menu::Models
        } else if i.starts_with("/effort ") {
            Menu::Effort
        } else if i.starts_with("/permissions ") {
            Menu::Permissions
        } else if i.starts_with("/skills ") {
            Menu::Skills
        } else if self.active_file_query().is_some() {
            Menu::Files
        } else if i.starts_with('/') && !i.contains(' ') {
            Menu::Commands
        } else {
            Menu::None
        }
    }

    /// Completion menu items according to the active submenu. Unified source:
    /// commands, models, sessions (dynamic), `/providers` levels.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        match self.menu_kind() {
            Menu::None => Vec::new(),
            Menu::Commands => COMMANDS
                .iter()
                .filter(|(name, _, _)| name.starts_with(self.input.as_str()))
                .map(|(name, desc, _)| MenuItem::new(name, name, desc, true))
                .collect(),
            Menu::Models => {
                let q = self.input.strip_prefix("/models ").unwrap_or("");
                let mut items = models()
                    .iter()
                    .filter(|meta| meta.slug.starts_with(q))
                    .map(|meta| {
                        MenuItem::new(
                            meta.slug,
                            meta.slug,
                            meta.incompatibility_reason.unwrap_or(meta.tag),
                            meta.incompatibility_reason.is_none(),
                        )
                    })
                    .collect::<Vec<_>>();
                if !q.trim().is_empty() && !models().iter().any(|meta| meta.slug == q) {
                    items.push(MenuItem::new(q, q, "descriptor unavailable", false));
                }
                items
            }
            Menu::Effort => {
                let q = self.input.strip_prefix("/effort ").unwrap_or("").trim();
                let q_lower = q.to_ascii_lowercase();
                let supported = supported_reasoning_efforts_for_model(&self.model);
                REASONING_EFFORTS
                    .iter()
                    .filter(|effort| {
                        supported
                            .iter()
                            .any(|supported| supported.eq_ignore_ascii_case(effort.id))
                    })
                    .filter(|effort| {
                        q.is_empty()
                            || effort.id.starts_with(&q_lower)
                            || effort.label.to_ascii_lowercase().contains(&q_lower)
                    })
                    .map(|effort| {
                        let mut hint = effort.hint.to_string();
                        if self
                            .reasoning_effort
                            .as_deref()
                            .is_some_and(|current| current.eq_ignore_ascii_case(effort.id))
                        {
                            hint = if hint.is_empty() {
                                "current".into()
                            } else {
                                format!("{hint} · current")
                            };
                        }
                        MenuItem::new(effort.id, effort.label, &hint, true)
                    })
                    .collect()
            }
            Menu::Permissions => {
                let q = self.input.strip_prefix("/permissions ").unwrap_or("");
                PERMISSION_MODES
                    .iter()
                    .filter(|mode| q.is_empty() || mode.id.starts_with(q) || mode.label.contains(q))
                    .map(|mode| {
                        let label = if mode.id == self.permission_mode {
                            format!("{} (current)", mode.label)
                        } else {
                            mode.label.to_string()
                        };
                        MenuItem::new(mode.id, &label, mode.hint, true)
                    })
                    .collect()
            }
            Menu::Resume => self
                .sessions
                .iter()
                .filter(|s| {
                    let q = self.input.strip_prefix("/resume ").unwrap_or("");
                    q.is_empty() || s.id.starts_with(q) || s.label.contains(q)
                })
                .map(|s| MenuItem {
                    id: s.id.clone(),
                    label: s.label.clone(),
                    hint: s.hint.clone(),
                    enabled: true,
                })
                .collect(),
            Menu::Skills => {
                let q = self.input.strip_prefix("/skills ").unwrap_or("");
                self.skills
                    .iter()
                    .filter(|name| name.contains(q))
                    .map(|name| MenuItem::new(name, name, "", true))
                    .collect()
            }
            Menu::Files => {
                let Some((_, q)) = self.active_file_query() else {
                    return Vec::new();
                };
                let mut items = self
                    .files
                    .iter()
                    .filter(|path| q.is_empty() || path.contains(q))
                    .take(20)
                    .map(|path| MenuItem::new(path, path, "file", true))
                    .collect::<Vec<_>>();
                if items.is_empty() {
                    items.push(MenuItem::new("", "No files", "", false));
                }
                items
            }
            Menu::ProviderAuth => {
                let q = self.input.strip_prefix("/providers ").unwrap_or("");
                AUTH_KINDS
                    .iter()
                    .filter(|(id, _, _)| id.starts_with(q))
                    .map(|(id, label, en)| {
                        MenuItem::new(id, label, if *en { "" } else { "coming soon" }, *en)
                    })
                    .collect()
            }
            Menu::ProviderList => {
                let q = self
                    .input
                    .strip_prefix("/providers subscription ")
                    .unwrap_or("");
                SUB_PROVIDERS
                    .iter()
                    .filter(|(id, _, _)| id.starts_with(q))
                    .map(|(id, label, en)| {
                        let hint = if *id == "codex" {
                            if self.provider_connected {
                                "connected"
                            } else {
                                "not connected"
                            }
                        } else if *en {
                            ""
                        } else {
                            "coming soon"
                        };
                        MenuItem::new(id, label, hint, *en)
                    })
                    .collect()
            }
            Menu::ProviderActions => {
                // Connect active only when disconnected; Disconnect the other way around.
                let c = self.provider_connected;
                vec![
                    MenuItem::new(
                        "connect",
                        "Connect",
                        if c { "already connected" } else { "" },
                        !c,
                    ),
                    MenuItem::new(
                        "disconnect",
                        "Disconnect",
                        if c { "" } else { "already disconnected" },
                        c,
                    ),
                ]
            }
            Menu::McpList => {
                let q = self.input.strip_prefix("/mcp ").unwrap_or("");
                if self.mcp_servers.is_empty() {
                    return vec![MenuItem::new(
                        "",
                        "No MCP servers",
                        "add .mcp.json to the workspace",
                        false,
                    )];
                }
                self.mcp_servers
                    .iter()
                    .filter(|m| m.name.starts_with(q))
                    .map(|m| {
                        let hint = match m.status {
                            McpStatus::Connected => {
                                format!("{} · connected · {} tools", m.source, m.tool_count)
                            }
                            McpStatus::Connecting => format!("{} · connecting...", m.source),
                            McpStatus::Failed => format!("{} · failed", m.source),
                            McpStatus::Disconnected if m.needs_trust => {
                                format!("{} · trust required", m.source)
                            }
                            McpStatus::Disconnected => format!("{} · not connected", m.source),
                        };
                        MenuItem::new(&m.name, &m.name, &hint, true)
                    })
                    .collect()
            }
            Menu::McpActions => {
                let srv = self.active_mcp_server();
                let meta = self.mcp_servers.iter().find(|m| m.name == srv);
                let needs_trust = meta.is_some_and(|m| m.needs_trust);
                let remote = meta.is_some_and(|m| m.remote);
                let status = meta.map(|m| m.status);
                let connecting = status == Some(McpStatus::Connecting);
                let mut items = if status == Some(McpStatus::Connected) {
                    vec![
                        MenuItem::new("disconnect", "Disconnect", "", true),
                        MenuItem::new("tools", "View tools", "", true),
                        MenuItem::new("resources", "View resources", "", true),
                        // What the server says about itself. Inspected here
                        // because it never reaches the model: a tool definition
                        // is not a tool output and would carry that prose past
                        // the taint defense.
                        MenuItem::new("info", "View server instructions", "", true),
                    ]
                } else if needs_trust {
                    vec![MenuItem::new(
                        "trust",
                        "Trust connect",
                        if connecting {
                            "connecting..."
                        } else {
                            "MCP tools not exposed"
                        },
                        false,
                    )]
                } else {
                    vec![MenuItem::new(
                        "connect",
                        "Connect",
                        if connecting {
                            "connecting..."
                        } else {
                            "MCP tools not exposed"
                        },
                        false,
                    )]
                };
                // Only a remote server has an authorization server to talk to.
                if remote {
                    items.push(MenuItem::new("login", "Authorize (OAuth)", "browser", true));
                    items.push(MenuItem::new("logout", "Forget authorization", "", true));
                }
                items
            }
        }
    }

    /// Is the completion menu open? (at least one item to offer).
    pub fn menu_open(&self) -> bool {
        !self.menu_items().is_empty()
    }

    /// No conversation yet (empty transcript): rendering shows the welcome
    /// screen (card + logo) instead of the thread. Back to the welcome after `/new`
    /// or `/clear`, which empty `blocks`.
    pub fn is_welcome(&self) -> bool {
        self.blocks.is_empty() && !self.shutdown_in_progress
    }

    /// Provider targeted by level 3 (`/providers subscription <provider> ...`).
    fn active_provider(&self) -> String {
        self.input
            .strip_prefix("/providers subscription ")
            .and_then(|r| r.split(' ').next())
            .unwrap_or("")
            .to_string()
    }

    /// MCP server targeted by level 2 (`/mcp <server> ...`). The name can contain
    /// spaces: we keep the longest known name that prefixes the input and is
    /// followed by a space.
    fn active_mcp_server(&self) -> String {
        let Some(rest) = self.input.strip_prefix("/mcp ") else {
            return String::new();
        };
        self.mcp_servers
            .iter()
            .map(|m| m.name.as_str())
            .filter(|name| rest.strip_prefix(*name).is_some_and(|r| r.starts_with(' ')))
            .max_by_key(|name| name.len())
            .unwrap_or("")
            .to_string()
    }

    fn active_file_query(&self) -> Option<(usize, &str)> {
        let prefix = self.input.get(..self.cursor).unwrap_or(&self.input);
        let start = prefix
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        let token = &prefix[start..];
        token.strip_prefix('@').map(|query| (start, query))
    }

    fn replace_file_mention(&mut self, path: &str) {
        let Some((start, _)) = self.active_file_query() else {
            return;
        };
        let replacement = format!("@{path} ");
        self.input.replace_range(start..self.cursor, &replacement);
        self.cursor = start + replacement.len();
    }

    /// Tab: completes the breadcrumb toward the selected item (goes down one
    /// level for items with a submenu, otherwise pre-fills the command).
    fn complete(&mut self, kind: Menu, item: &MenuItem) {
        let provider = self.active_provider();
        let value = match kind {
            Menu::Commands => format!("{} ", item.id),
            Menu::Models if !item.enabled => return,
            Menu::Models => format!("/models {}", item.id),
            Menu::Effort => format!("/effort {}", item.id),
            Menu::Permissions => format!("/permissions {}", item.id),
            Menu::Skills => format!("/{} ", item.id),
            Menu::ProviderAuth if item.id == "subscription" => "/providers subscription ".into(),
            Menu::ProviderAuth => format!("/providers {} ", item.id),
            // Wired provider -> go down to the actions; otherwise pre-fill.
            Menu::ProviderList if item.enabled => format!("/providers subscription {} ", item.id),
            Menu::ProviderList => format!("/providers subscription {}", item.id),
            Menu::ProviderActions => format!("/providers subscription {provider} {}", item.id),
            Menu::Files if item.enabled => {
                self.replace_file_mention(&item.id);
                return;
            }
            Menu::Files => return,
            Menu::McpList if item.enabled => format!("/mcp {} ", item.id),
            Menu::McpActions if !item.enabled => return,
            Menu::McpActions => format!("/mcp {} {}", self.active_mcp_server(), item.id),
            Menu::McpList | Menu::Resume | Menu::None => return,
        };
        self.set_input(value);
    }

    /// Enter: runs the selected item, or goes down one level when it opens a
    /// submenu (command with an argument, `subscription`), or inserts (skill).
    fn activate(&mut self, kind: Menu, item: MenuItem) -> InputAction {
        match kind {
            Menu::None => InputAction::None,
            Menu::Commands => {
                if command_takes_arg(&item.id) {
                    self.set_input(format!("{} ", item.id));
                    InputAction::None
                } else {
                    self.clear_input();
                    InputAction::Command(item.id)
                }
            }
            Menu::Models if item.enabled => {
                self.clear_input();
                InputAction::Command(format!("/models {}", item.id))
            }
            Menu::Models => InputAction::None,
            Menu::Effort => {
                self.clear_input();
                InputAction::Command(format!("/effort {}", item.id))
            }
            Menu::Permissions => {
                self.clear_input();
                InputAction::Command(format!("/permissions {}", item.id))
            }
            Menu::Resume => {
                self.clear_input();
                InputAction::Command(format!("/resume {}", item.id))
            }
            Menu::Skills => {
                // INSERTION (no execution): `/<skill> ` replaces the typed
                // `/skills...`, cursor right after; the user continues their message.
                self.set_input(format!("/{} ", item.id));
                InputAction::None
            }
            Menu::Files if item.enabled => {
                self.replace_file_mention(&item.id);
                InputAction::None
            }
            Menu::Files => InputAction::None,
            Menu::ProviderAuth if item.id == "subscription" => {
                self.set_input("/providers subscription ".into());
                InputAction::None
            }
            Menu::ProviderAuth => {
                self.clear_input();
                InputAction::Command(format!("/providers {}", item.id))
            }
            Menu::ProviderList if item.enabled => {
                // Wired provider -> go down to the actions menu (connect/disconnect).
                self.set_input(format!("/providers subscription {} ", item.id));
                InputAction::None
            }
            Menu::ProviderList => {
                self.clear_input();
                InputAction::Command(format!("/providers subscription {}", item.id))
            }
            Menu::ProviderActions => {
                let provider = self.active_provider();
                self.clear_input();
                InputAction::Command(format!("/providers subscription {provider} {}", item.id))
            }
            // Selecting a server -> go down to the actions menu (connect/disconnect).
            Menu::McpList if item.enabled => {
                self.set_input(format!("/mcp {} ", item.id));
                InputAction::None
            }
            Menu::McpList => InputAction::None,
            Menu::McpActions if !item.enabled => InputAction::None,
            Menu::McpActions => {
                let server = self.active_mcp_server();
                self.clear_input();
                InputAction::Command(format!("/mcp {server} {}", item.id))
            }
        }
    }

    /// Key handling. While waiting for a permission, only y/n/Enter/Esc/Ctrl+C count.
    pub fn on_key(&mut self, key: KeyEvent) -> InputAction {
        let is_ctrl_c = is_ctrl_key(&key, 'c');
        let is_ctrl_t = is_ctrl_key(&key, 't');
        if !is_ctrl_c {
            self.clear_quit_shortcut_hint();
        }

        if self.transcript_overlay_open {
            return self.on_transcript_overlay_key(key, is_ctrl_t, is_ctrl_c);
        }

        if is_ctrl_t && !self.shutdown_in_progress {
            self.shortcut_overlay_open = false;
            self.open_transcript_overlay();
            return InputAction::None;
        }

        // A permission dialog replaces the composer: the overlay is dropped
        // rather than left armed to reappear once the dialog is answered.
        if self.pending.is_some() {
            self.shortcut_overlay_open = false;
        } else if self.on_shortcut_overlay_key(&key) {
            return InputAction::None;
        }

        if let Some(prompt) = &self.pending {
            // US-009 AC1/AC2: the session options only exist when the request
            // declares itself memoizable; otherwise their keys do nothing.
            let memoizable = prompt.memoizable;
            return match key.code {
                KeyCode::Char('o') | KeyCode::Char('y') | KeyCode::Enter => {
                    self.pending = None;
                    InputAction::Permission {
                        allow: true,
                        remember: false,
                    }
                }
                KeyCode::Char('a') if memoizable => {
                    self.pending = None;
                    InputAction::Permission {
                        allow: true,
                        remember: true,
                    }
                }
                KeyCode::Char('d') if memoizable => {
                    self.pending = None;
                    InputAction::Permission {
                        allow: false,
                        remember: true,
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.pending = None;
                    InputAction::Permission {
                        allow: false,
                        remember: false,
                    }
                }
                _ if is_ctrl_c => {
                    self.pending = None;
                    self.clear_quit_shortcut_hint();
                    InputAction::Permission {
                        allow: false,
                        remember: false,
                    }
                }
                // US-009 AC5: any other key leaves the dialog open.
                _ => InputAction::None,
            };
        }

        // Completion menu open (commands or submenus): arrows / Tab /
        // Enter / Esc are dedicated to it.
        if self.menu_open() {
            let items = self.menu_items();
            let idx = self.completion_index.min(items.len().saturating_sub(1));
            let kind = self.menu_kind();
            match key.code {
                KeyCode::Up => {
                    self.completion_index = idx.saturating_sub(1);
                    return InputAction::None;
                }
                KeyCode::Down => {
                    self.completion_index = (idx + 1).min(items.len().saturating_sub(1));
                    return InputAction::None;
                }
                KeyCode::Tab => {
                    if let Some(item) = items.get(idx) {
                        self.complete(kind, item);
                        self.completion_index = 0;
                    }
                    return InputAction::None;
                }
                // BARE Enter only: Alt/Shift+Enter insert a newline
                // even when the menu is open (US-009 AC1).
                KeyCode::Enter if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                    self.completion_index = 0;
                    if let Some(item) = items.get(idx).cloned() {
                        return self.activate(kind, item);
                    }
                    return InputAction::None;
                }
                KeyCode::Esc => {
                    self.clear_input();
                    self.completion_index = 0;
                    return InputAction::None;
                }
                _ if is_ctrl_c => {
                    self.clear_input();
                    self.completion_index = 0;
                    self.clear_quit_shortcut_hint();
                    return InputAction::None;
                }
                _ => {}
            }
        }

        if is_ctrl_c {
            return self.on_ctrl_c();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                // Ctrl+J (0x0A in raw mode -> `Char('j')`) is the universal
                // insertion shortcut: it depends on no extended keyboard
                // protocol, unlike Shift+Enter.
                KeyCode::Char('j') | KeyCode::Enter => {
                    self.insert_newline();
                    self.completion_index = 0;
                    InputAction::None
                }
                KeyCode::Char('a') => {
                    self.move_home();
                    InputAction::None
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    InputAction::None
                }
                KeyCode::Char('u') => {
                    self.clear_input();
                    self.completion_index = 0;
                    InputAction::None
                }
                KeyCode::Char('w') => {
                    self.delete_prev_word();
                    self.completion_index = 0;
                    InputAction::None
                }
                _ => InputAction::None,
            };
        }

        match key.code {
            KeyCode::Esc if self.status == Status::Thinking && key.modifiers.is_empty() => {
                InputAction::Interrupt
            }
            // Alt+Enter, and Shift+Enter on terminals that report the
            // modifier: newline, no submission (US-009 AC1/AC2).
            KeyCode::Enter if key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                self.insert_newline();
                self.completion_index = 0;
                InputAction::None
            }
            KeyCode::Enter => {
                let text = self.expand_pastes(self.input.trim());
                if text.is_empty() {
                    InputAction::None
                } else if is_command(&text) {
                    // Real Pyxis command (1st word in COMMANDS, e.g. `/models ...`).
                    self.clear_input();
                    self.completion_index = 0;
                    InputAction::Command(text)
                } else {
                    // Everything else (including a message starting with `/<skill> ...`)
                    // is sent to the agent.
                    self.clear_input();
                    InputAction::Submit(text)
                }
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                self.completion_index = 0;
                InputAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                self.completion_index = 0;
                InputAction::None
            }
            KeyCode::Delete => {
                self.delete();
                self.completion_index = 0;
                InputAction::None
            }
            // Cursor moves inside the input.
            KeyCode::Left => {
                self.move_left();
                InputAction::None
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::None
            }
            KeyCode::Home => {
                self.move_home();
                InputAction::None
            }
            KeyCode::End => {
                self.move_end();
                InputAction::None
            }
            // Arrows (menu closed): navigation between the input lines,
            // then history recall once the first/last line is
            // reached (US-009 AC4).
            KeyCode::Up => {
                if !self.move_line_up() {
                    self.history_prev();
                }
                InputAction::None
            }
            KeyCode::Down => {
                if !self.move_line_down() {
                    self.history_next();
                }
                InputAction::None
            }
            KeyCode::PageUp => {
                self.scroll_up(5);
                InputAction::ScrollUp
            }
            KeyCode::PageDown => {
                self.scroll_down(5);
                InputAction::ScrollDown
            }
            _ => InputAction::None,
        }
    }

    fn on_transcript_overlay_key(
        &mut self,
        key: KeyEvent,
        is_ctrl_t: bool,
        is_ctrl_c: bool,
    ) -> InputAction {
        if is_ctrl_t || is_ctrl_c || is_plain_char_key(&key, 'q') || key.code == KeyCode::Esc {
            self.close_transcript_overlay();
            self.clear_quit_shortcut_hint();
            return InputAction::None;
        }

        let page = self.transcript_overlay_page_height();
        match key.code {
            KeyCode::Up if key.modifiers.is_empty() => self.transcript_overlay_scroll_up(1),
            KeyCode::Down if key.modifiers.is_empty() => self.transcript_overlay_scroll_down(1),
            KeyCode::PageUp => self.transcript_overlay_scroll_up(page),
            KeyCode::PageDown => self.transcript_overlay_scroll_down(page),
            KeyCode::Home => self.jump_transcript_overlay_top(),
            KeyCode::End => self.jump_transcript_overlay_bottom(),
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.transcript_overlay_scroll_up(page)
            }
            KeyCode::Char(' ') if key.modifiers.is_empty() => {
                self.transcript_overlay_scroll_down(page)
            }
            _ if is_plain_char_key(&key, 'k') => self.transcript_overlay_scroll_up(1),
            _ if is_plain_char_key(&key, 'j') => self.transcript_overlay_scroll_down(1),
            _ if is_ctrl_key(&key, 'b') => self.transcript_overlay_scroll_up(page),
            _ if is_ctrl_key(&key, 'f') => self.transcript_overlay_scroll_down(page),
            _ if is_ctrl_key(&key, 'u') => {
                self.transcript_overlay_scroll_up((page.saturating_add(1)) / 2)
            }
            _ if is_ctrl_key(&key, 'd') => {
                self.transcript_overlay_scroll_down((page.saturating_add(1)) / 2)
            }
            _ => {}
        }
        InputAction::None
    }
}

/// Summary of a line of the aggregated turn diff (US-018): the answer to "what
/// changed?" without replaying a diff already seen edit by edit. The
/// files touched by a shell command, in contrast, were never displayed
/// anywhere else: that is where the line earns its cost.
/// Session facts the frontend does not hold: they belong to the process, not to
/// the display state (US-005).
#[derive(Debug, Clone, Copy)]
pub struct SessionFacts<'a> {
    /// Name of the persistence file of the current session.
    pub session_id: &'a str,
    /// Scope of the active sandbox, as the binary resolved it.
    pub sandbox: &'a str,
    /// Configuration layer each displayed value comes from (US-005 AC2), keyed by
    /// the `SOURCE_KEY_*` vocabulary. A key absent from here is at its default
    /// value, or was changed in session, and no layer describes it any more.
    pub config_sources: &'a [(&'static str, &'static str)],
    /// Profile applied to this session (US-006), when one was selected. A profile
    /// changes four keys at once and is otherwise invisible in the values.
    pub profile: Option<&'a str>,
    /// Orchestration facts (US-019 AC3). Local by construction: they come from
    /// the runtime's last-state signal and from its v1 constants, so `/status`
    /// still answers without a single request.
    pub runtime: RuntimeFacts,
}

/// The bounds a thread runs under, and how much of them it is using.
///
/// Every limit is a CONSTANT of the runtime: FR-20 forbids a configuration key
/// for orchestration in v1, so showing them here is showing the whole truth.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeFacts {
    pub active_agents: usize,
    pub max_active_agents: usize,
    pub max_agents_per_root: usize,
    pub max_agent_depth: usize,
    pub command_mailbox: usize,
    pub max_pending_inputs: usize,
}

/// Keys of `SessionFacts::config_sources`. They are the configuration key names,
/// so that the producer and this renderer share one vocabulary instead of two
/// sets of literals.
pub const SOURCE_KEY_MODEL: &str = "model";
pub const SOURCE_KEY_REASONING_EFFORT: &str = "reasoning_effort";
pub const SOURCE_KEY_PERMISSION_MODE: &str = "permission_mode";
pub const SOURCE_KEY_SANDBOX_MODE: &str = "sandbox_mode";

/// ` (from <layer>)`, or nothing when no layer owns the key.
fn source_suffix(sources: &[(&'static str, &'static str)], key: &str) -> String {
    sources
        .iter()
        .find(|(owned, _)| *owned == key)
        .map(|(_, layer)| format!(" (from {layer})"))
        .unwrap_or_default()
}

/// Marker for a piece of information the session does not have. Written out
/// rather than omitting the line: a missing line reads as "nothing to report",
/// which is a different statement (US-005 AC3).
const UNAVAILABLE: &str = "unavailable";

/// US-005: session configuration, read from the local state only. No network
/// call, and the result is displayed as a notice, so it never enters the
/// transcript sent to the model.
pub fn session_status_report(state: &AppState, facts: SessionFacts<'_>) -> String {
    let effort = state
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(UNAVAILABLE);
    let workspace = if state.workspace.is_empty() {
        UNAVAILABLE
    } else {
        &state.workspace
    };
    let session = if facts.session_id.is_empty() {
        UNAVAILABLE
    } else {
        facts.session_id
    };
    let sandbox = if facts.sandbox.is_empty() {
        UNAVAILABLE
    } else {
        facts.sandbox
    };
    // Built line by line rather than in one format string: each configured line
    // carries the layer it comes from, and only when a layer owns it.
    let sources = facts.config_sources;
    let mut report = String::from("Session status");
    report.push_str(&format!(
        "\n  model: {}{}",
        state.model,
        source_suffix(sources, SOURCE_KEY_MODEL)
    ));
    report.push_str(&format!(
        "\n  reasoning effort: {effort}{}",
        source_suffix(sources, SOURCE_KEY_REASONING_EFFORT)
    ));
    report.push_str(&format!(
        "\n  permissions: {}{}",
        state.permission_mode_label(),
        source_suffix(sources, SOURCE_KEY_PERMISSION_MODE)
    ));
    report.push_str(&format!(
        "\n  sandbox: {sandbox}{}",
        source_suffix(sources, SOURCE_KEY_SANDBOX_MODE)
    ));
    if let Some(profile) = facts.profile {
        report.push_str(&format!("\n  profile: {profile}"));
    }
    report.push_str(&format!("\n  workspace: {workspace}"));
    report.push_str(&format!("\n  session: {session}"));

    // US-019 AC3: thread, turn, state, queue depth, sub-agents and the fixed
    // limits. Each on its own short line so a 40-column terminal wraps at most
    // the identifier, never the label that explains it.
    let thread = if state.thread_id.is_empty() {
        UNAVAILABLE
    } else {
        &state.thread_id
    };
    report.push_str(&format!("\n  thread: {thread}"));
    report.push_str(&format!(
        "\n  turn: {}",
        match (&state.turn_id, &state.turn_state) {
            (Some(id), Some(turn_state)) => format!("{id} ({turn_state})"),
            (Some(id), None) => id.clone(),
            _ => "none yet".to_string(),
        }
    ));
    let runtime = facts.runtime;
    report.push_str(&format!(
        "\n  pending inputs: {} (max {} per turn, mailbox {})",
        state.pending_inputs, runtime.max_pending_inputs, runtime.command_mailbox
    ));
    report.push_str(&format!(
        "\n  sub-agents: {} active (max {} active, {} created, depth {})",
        runtime.active_agents,
        runtime.max_active_agents,
        runtime.max_agents_per_root,
        runtime.max_agent_depth
    ));
    report
}

/// US-005: consumption of the session. Same rule as above: everything comes
/// from what the backend already reported, nothing is estimated, and an absent
/// measure is named.
pub fn session_usage_report(state: &AppState) -> String {
    let context = match (state.context_tokens, state.context_window) {
        (Some(tokens), Some(window)) => format!(
            "{tokens} / {window} tokens{}",
            state
                .context_pct
                .map(|pct| format!(" ({pct}%)"))
                .unwrap_or_default()
        ),
        (Some(tokens), None) => {
            format!("{tokens} tokens ({UNAVAILABLE}: context window of the model unknown)")
        }
        (None, _) => format!("{UNAVAILABLE}: no usage reported by the backend yet"),
    };
    format!(
        "Session usage\n  input tokens: {}\n  output tokens: {}\n  context: {context}\n  quota: {}",
        state.total_input_tokens,
        state.total_output_tokens,
        quota_line(state.quota.as_ref()),
    )
}

fn quota_line(quota: Option<&agent_core::quota::QuotaSnapshot>) -> String {
    let Some(window) = quota.and_then(|snapshot| snapshot.most_consumed()) else {
        return format!("{UNAVAILABLE}: not reported by the backend");
    };
    let scope = window
        .window_label()
        .map(|label| format!(" ({label})"))
        .unwrap_or_default();
    let reset = window
        .resets_at_label()
        .map(|instant| format!(", resets at {instant}"))
        .unwrap_or_else(|| format!(", reset time {UNAVAILABLE}"));
    format!("{:.0}% used{scope}{reset}", window.used_percent)
}

/// Wire name of a plan step status (US-009). Single source for the cache
/// fingerprint and for anything that has to name a status in text.
pub fn plan_status_label(status: agent_core::PlanStatus) -> &'static str {
    match status {
        agent_core::PlanStatus::Pending => "pending",
        agent_core::PlanStatus::InProgress => "in_progress",
        agent_core::PlanStatus::Completed => "completed",
    }
}

/// Glyph of a plan step: done, in progress, still to do. Deliberately three
/// distinct shapes rather than colors alone, so the state survives a monochrome
/// terminal.
pub fn plan_status_glyph(status: agent_core::PlanStatus) -> &'static str {
    match status {
        agent_core::PlanStatus::Completed => "✔",
        agent_core::PlanStatus::InProgress => "▸",
        agent_core::PlanStatus::Pending => "□",
    }
}

pub fn turn_diff_summary(view: &TurnDiffView) -> String {
    let (added, removed) = view.totals();
    let n = view.files.len();
    let plural = if n == 1 { "" } else { "s" };
    if n <= 3 {
        let names: Vec<&str> = view.files.iter().map(|f| f.path.as_str()).collect();
        return format!(
            "{n} file{plural} changed (+{added} -{removed}): {}",
            names.join(", ")
        );
    }
    format!("{n} file{plural} changed (+{added} -{removed})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{ToolCallView, ToolOutputDeltaView, ToolResultView};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn streamed_text_accumulates_into_one_assistant_block() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("Bon".into()));
        s.apply(&AgentEvent::Text("jour".into()));
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(
            s.blocks[0],
            Block::Assistant {
                text: "Bonjour".into(),
                streaming: true
            }
        );
        s.apply(&AgentEvent::EndTurn);
        assert!(matches!(
            s.blocks[0],
            Block::Assistant {
                streaming: false,
                ..
            }
        ));
        assert_eq!(s.status, Status::Idle);
    }

    #[test]
    fn stream_reset_removes_uncommitted_blocks() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("prefix".into()));
        s.apply(&AgentEvent::Reasoning("raison".into()));
        s.apply(&AgentEvent::StreamReset);
        assert!(s.blocks.is_empty());
        assert_eq!(s.turn_chars, 0);
        s.apply(&AgentEvent::Text("final".into()));
        s.apply(&AgentEvent::EndTurn);
        assert_eq!(
            s.blocks,
            vec![Block::Assistant {
                text: "final".into(),
                streaming: false
            }]
        );
    }

    #[test]
    fn tool_call_finalizes_assistant_and_records_summary() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("je lance".into()));
        s.apply(&AgentEvent::ToolCall(ToolCallView {
            id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "ls -la" }),
        }));
        assert!(matches!(
            s.blocks[0],
            Block::Assistant {
                streaming: false,
                ..
            }
        ));
        assert_eq!(
            s.blocks[1],
            Block::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "ls -la" }),
                input_hash: crate::cache::value_hash(&serde_json::json!({ "command": "ls -la" })),
            }
        );
    }

    fn delta(id: &str, chunk: &str) -> AgentEvent {
        AgentEvent::ToolOutputDelta(ToolOutputDeltaView {
            id: id.into(),
            chunk: chunk.into(),
        })
    }

    fn tool_call(id: &str) -> AgentEvent {
        AgentEvent::ToolCall(ToolCallView {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "cargo build" }),
        })
    }

    #[test]
    fn live_output_shows_while_the_tool_runs_and_clears_on_result() {
        // US-015 AC2: the fragments are displayed under the running call; the
        // final result replaces them.
        let mut s = AppState::new("gpt-5", false);
        s.apply(&tool_call("c1"));
        s.apply(&delta("c1", "Compiling agent-core\n"));
        s.apply(&delta("c1", "Compiling agent-tui\n"));
        assert_eq!(
            s.live_output_lines(),
            vec![
                "Compiling agent-core".to_string(),
                "Compiling agent-tui".to_string()
            ]
        );
        s.apply(&AgentEvent::ToolResult(ToolResultView {
            id: "c1".into(),
            content: "done".into(),
            status: None,
            structured_content: None,
            is_error: false,
            untrusted: true,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        assert!(s.live_output_lines().is_empty());
    }

    #[test]
    fn live_output_survives_an_interruption() {
        // US-015 AC4: the synthetic interruption result does not carry the
        // output already produced, which must stay readable.
        let mut s = AppState::new("gpt-5", false);
        s.apply(&tool_call("c1"));
        s.apply(&delta("c1", "warning: unused\n"));
        s.apply(&AgentEvent::ToolResult(ToolResultView {
            id: "c1".into(),
            content: agent_core::INTERRUPTED_TOOL_RESULT.into(),
            status: None,
            structured_content: None,
            is_error: true,
            untrusted: false,
            error_kind: Some(ToolErrorKind::Semantic),
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        s.apply(&AgentEvent::Interrupted);
        assert_eq!(s.live_output_lines(), vec!["warning: unused".to_string()]);
    }

    #[test]
    fn live_output_is_bounded_and_sanitized() {
        // AC3: the display stays bounded whatever the produced volume, and an
        // ANSI sequence in the output cannot alter the rendering.
        let mut s = AppState::new("gpt-5", false);
        s.apply(&tool_call("c1"));
        for i in 0..500 {
            s.apply(&delta("c1", &format!("line{i}\n")));
        }
        let lines = s.live_output_lines();
        assert_eq!(lines.len(), LIVE_OUTPUT_MAX_LINES);
        assert!(
            lines.last().is_some_and(|l| l.contains("line499")),
            "{lines:?}"
        );

        s.apply(&delta("c1", "\x1b[2J\x1b]0;titre\x07danger\n"));
        let lines = s.live_output_lines();
        assert!(
            lines.iter().all(|l| !l.contains('\x1b')),
            "aucune séquence d'échappement ne doit survivre: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("danger")), "{lines:?}");
    }

    #[test]
    fn live_output_resets_on_the_next_tool_call() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&tool_call("c1"));
        s.apply(&delta("c1", "premier\n"));
        s.apply(&tool_call("c2"));
        assert!(s.live_output_lines().is_empty());
        s.apply(&delta("c2", "second\n"));
        assert_eq!(s.live_output_lines(), vec!["second".to_string()]);
    }

    #[test]
    fn tool_result_carries_taint_and_error() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::ToolResult(ToolResultView {
            id: "c1".into(),
            content: "oops".into(),
            status: None,
            structured_content: None,
            is_error: true,
            untrusted: true,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        assert_eq!(
            s.blocks[0],
            Block::ToolResult {
                call_id: "c1".into(),
                content: "oops".into(),
                untrusted: true,
                is_error: true,
                error_kind: None
            }
        );
    }

    #[test]
    fn typing_and_submit_produces_action_and_clears_input() {
        let mut s = AppState::new("gpt-5", false);
        for c in "salut".chars() {
            assert_eq!(s.on_key(key(c)), InputAction::None);
        }
        assert_eq!(s.input, "salut");
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Submit("salut".into()));
        assert!(s.input.is_empty());
    }

    #[test]
    fn empty_submit_is_noop() {
        let mut s = AppState::new("gpt-5", false);
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
    }

    #[test]
    fn slash_opens_and_filters_command_menu() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        assert!(s.menu_open(), "menu should open on /");
        assert_eq!(s.menu_items().len(), COMMANDS.len());
        s.on_key(key('m'));
        // "/m" matches /models AND /mcp.
        let m = s.menu_items();
        assert_eq!(m.len(), 2, "/m matches /models and /mcp");
        assert!(m.iter().all(|it| it.id.starts_with("/m")));
        // "/mo" disambiguates to /models alone.
        s.on_key(key('o'));
        let m = s.menu_items();
        assert_eq!(m.len(), 1, "«/mo» ne matche que /models");
        assert_eq!(m[0].id, "/models");
    }

    #[test]
    fn permissions_submenu_marks_current_and_routes_selection() {
        let mut s = AppState::new("gpt-5", false);
        s.set_permission_mode("read-only");
        s.set_input("/permissions ".into());

        let items = s.menu_items();
        assert_eq!(items.len(), PERMISSION_MODES.len());
        let current = items.iter().find(|item| item.id == "read-only").unwrap();
        assert!(current.label.contains("(current)"));

        s.set_input("/permissions full".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "full-access");
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Command("/permissions full-access".into())
        );
    }

    #[test]
    fn mcp_submenu_lists_servers_with_status_badges() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![
            McpServerMeta {
                name: "filesystem".into(),
                status: McpStatus::Connected,
                source: "workspace".into(),
                needs_trust: false,
                tool_count: 3,
                remote: false,
            },
            McpServerMeta {
                name: "fetch".into(),
                status: McpStatus::Disconnected,
                source: "user".into(),
                needs_trust: false,
                tool_count: 0,
                remote: false,
            },
        ];
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        let items = s.menu_items();
        assert_eq!(items.len(), 2);
        let fs = items.iter().find(|i| i.id == "filesystem").unwrap();
        assert!(fs.hint.contains("connected"), "connected status expected");
        assert!(fs.hint.contains("3 tools"));
        let fetch = items.iter().find(|i| i.id == "fetch").unwrap();
        assert_eq!(fetch.hint, "user · not connected");
    }

    #[test]
    fn mcp_server_selection_descends_to_disabled_connect() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "fetch".into(),
            status: McpStatus::Disconnected,
            source: "user".into(),
            needs_trust: false,
            tool_count: 0,
            remote: false,
        }];
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        // Enter on the server -> goes down to the actions menu (does not execute).
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
        assert_eq!(s.input, "/mcp fetch ");
        // Disconnected: connect visible but inactive, because MCP tools are
        // not exposed to the model in this build.
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "connect");
        assert!(!items[0].enabled);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn mcp_workspace_server_routes_through_trust_action() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "local".into(),
            status: McpStatus::Disconnected,
            source: "workspace".into(),
            needs_trust: true,
            tool_count: 0,
            remote: false,
        }];
        s.set_input("/mcp local ".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "trust");
        assert!(!items[0].enabled);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn mcp_connected_server_offers_disconnect_and_tools() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "fs".into(),
            status: McpStatus::Connected,
            source: "workspace".into(),
            needs_trust: false,
            tool_count: 2,
            remote: false,
        }];
        s.set_input("/mcp fs ".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["disconnect", "tools", "resources", "info"]);
    }

    /// Only a remote server has an authorization server to talk to, so a stdio
    /// one is never offered a login it could not run.
    #[test]
    fn only_a_remote_server_offers_the_oauth_actions() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "remote".into(),
            status: McpStatus::Connected,
            source: "user".into(),
            needs_trust: false,
            tool_count: 1,
            remote: true,
        }];
        s.set_input("/mcp remote ".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![
                "disconnect",
                "tools",
                "resources",
                "info",
                "login",
                "logout"
            ]
        );
    }

    #[test]
    fn mcp_server_name_with_space_reaches_actions() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "my server".into(),
            status: McpStatus::Connected,
            source: "workspace".into(),
            needs_trust: false,
            tool_count: 1,
            remote: false,
        }];
        // complete() writes the full name (with a space); the menu must switch to
        // actions, not stay stuck on the list (review regression #7).
        s.set_input("/mcp my server ".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["disconnect", "tools", "resources", "info"]);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/mcp my server disconnect".into())
        );
    }

    #[test]
    fn mcp_empty_registry_shows_disabled_placeholder() {
        let mut s = AppState::new("gpt-5", false);
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert!(!items[0].enabled, "placeholder non sélectionnable");
        // Enter on the placeholder dispatches nothing.
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    /// US-019: the five commands of the epic are in the single source, and each
    /// executes on Enter rather than opening a submenu (`force` and `clear` are
    /// typed, not picked).
    #[test]
    fn the_session_commands_of_us019_are_declared_and_directly_executable() {
        for name in ["/init", "/fork", "/copy", "/logout", "/hooks"] {
            let entry = COMMANDS.iter().find(|(id, _, _)| *id == name);
            assert!(entry.is_some(), "{name} absente de COMMANDS");
            if let Some((_, description, takes_arg)) = entry {
                assert!(!description.is_empty(), "{name} sans description");
                assert!(!takes_arg, "{name} ne doit pas ouvrir de sous-menu");
            }
        }
        // `/copy` must not steal the `/c` prefix from `/compact` and `/clear`.
        let mut s = AppState::new("gpt-5", false);
        s.set_input("/co".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|it| it.id).collect();
        assert!(ids.contains(&"/compact".to_string()), "{ids:?}");
        assert!(ids.contains(&"/copy".to_string()), "{ids:?}");
    }

    #[test]
    fn enter_on_non_arg_command_executes() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        // Navigates to /quit (without depending on the exact order of COMMANDS).
        let quit_idx = COMMANDS.iter().position(|(n, _, _)| *n == "/quit").unwrap();
        for _ in 0..quit_idx {
            s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/quit".into()));
        assert!(s.input.is_empty());
    }

    #[test]
    fn goal_command_highlighted_and_routed() {
        // `/goal` is a real command (routed), not an agent message.
        let mut s = AppState::new("gpt-5", false);
        for c in "/goal vivre de mes produits".chars() {
            s.on_key(key(c));
        }
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/goal vivre de mes produits".into())
        );
    }

    #[test]
    fn skills_submenu_inserts_and_routes_to_agent() {
        let mut s = AppState::new("gpt-5", false);
        s.skills = vec!["frontend-design".into(), "meta-code".into()];
        // Opens the skills submenu, filters by substring.
        s.input = "/skills front".into();
        s.cursor = s.input.len();
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "frontend-design");
        // Selection -> INSERTS `/frontend-design ` (no Command), cursor at the end.
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
        assert_eq!(s.input, "/frontend-design ");
        assert_eq!(s.cursor, s.input.len());
        // Submitted with a message -> goes to the AGENT (not a Pyxis command).
        for c in "refais l'UI".chars() {
            s.on_key(key(c));
        }
        let submit = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submit,
            InputAction::Submit("/frontend-design refais l'UI".into())
        );
    }

    #[test]
    fn file_mentions_filter_insert_and_submit_to_agent() {
        let mut s = AppState::new("gpt-5", false);
        s.files = vec!["crates/agent-tui/src/state.rs".into(), "README.md".into()];
        s.set_input("@state".into());

        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "crates/agent-tui/src/state.rs");
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "@crates/agent-tui/src/state.rs ");

        for c in "explique".chars() {
            s.on_key(key(c));
        }
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Submit("@crates/agent-tui/src/state.rs explique".into())
        );
    }

    #[test]
    fn cursor_inserts_in_middle_and_moves() {
        let mut s = AppState::new("gpt-5", false);
        for c in "helo".chars() {
            s.on_key(key(c));
        }
        // cursor at the end (4); step back by 1 (between 'l' and 'o') and insert 'l'.
        s.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        s.on_key(key('l'));
        assert_eq!(s.input, "hello");
        assert_eq!(s.cursor, 4);
        // Home then Backspace does nothing (cursor at the start).
        s.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        s.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s.input, "hello");
        // Delete removes the char under the cursor ('h').
        s.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(s.input, "ello");
    }

    #[test]
    fn unicode_cursor_moves_and_deletes_graphemes() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_str("a¢🙂");
        assert_eq!(s.cursor, "a¢🙂".len());

        s.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(s.cursor, "a¢".len());

        s.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s.input, "a🙂");
        assert_eq!(s.cursor, "a".len());
    }

    #[test]
    fn ctrl_shortcuts_edit_without_inserting_control_chars() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_str("hello world");

        s.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        s.on_key(key('>'));
        assert_eq!(s.input, ">hello world");

        s.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        s.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(s.input, ">hello ");

        s.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(s.input.is_empty());
    }

    #[test]
    fn providers_menu_three_levels_and_badge() {
        let mut s = AppState::new("gpt-5", true);
        s.provider_connected = true;
        // Level 1: auth types.
        s.input = "/providers ".into();
        let lvl1 = s.menu_items();
        assert_eq!(lvl1.len(), AUTH_KINDS.len());
        assert_eq!(lvl1[0].id, "subscription");
        assert!(!lvl1[1].enabled, "API key inactive");
        // "subscription" goes down to level 2 (providers).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/providers subscription ");
        let lvl2 = s.menu_items();
        assert_eq!(lvl2[0].id, "codex");
        assert_eq!(lvl2[0].hint, "connected", "connected badge on codex");
        // Codex (wired) goes down to level 3 (actions).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/providers subscription codex ");
        let lvl3 = s.menu_items();
        // Connected -> Connect greyed out, Disconnect active.
        assert_eq!(lvl3[0].id, "connect");
        assert!(!lvl3[0].enabled, "Connect disabled while connected");
        assert_eq!(lvl3[1].id, "disconnect");
        assert!(lvl3[1].enabled, "Disconnect enabled while connected");
        // Selecting Disconnect -> runs the full command.
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/providers subscription codex disconnect".into())
        );
    }

    #[test]
    fn provider_actions_invert_when_disconnected() {
        let mut s = AppState::new("gpt-5", true);
        s.provider_connected = false;
        s.input = "/providers subscription codex ".into();
        let lvl3 = s.menu_items();
        assert!(lvl3[0].enabled, "Connect enabled while disconnected");
        assert!(!lvl3[1].enabled, "Disconnect disabled while disconnected");
    }

    #[test]
    fn arrow_keys_navigate_prompt_history() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("premier");
        s.push_user("second");
        // draft being typed
        for c in "brou".chars() {
            s.on_key(key(c));
        }
        let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        // Up -> most recent; the draft is saved.
        s.on_key(up());
        assert_eq!(s.input, "second");
        s.on_key(up());
        assert_eq!(s.input, "premier");
        s.on_key(up()); // stops on the oldest (no wrap)
        assert_eq!(s.input, "premier");
        s.on_key(down());
        assert_eq!(s.input, "second");
        s.on_key(down()); // past the most recent -> draft restored
        assert_eq!(s.input, "brou");
    }

    #[test]
    fn history_ignores_consecutive_duplicates() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("x");
        s.push_user("x");
        s.push_user("y");
        assert_eq!(s.history, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn prompts_from_messages_keeps_user_only() {
        let msgs = vec![
            Message::user("q1"),
            Message::assistant_text("a1"),
            Message::user("q2"),
        ];
        assert_eq!(
            prompts_from_messages(&msgs),
            vec!["q1".to_string(), "q2".to_string()]
        );
    }

    #[test]
    fn resume_submenu_lists_sessions_and_routes_id() {
        let mut s = AppState::new("gpt-5", false);
        s.sessions = vec![
            SessionMeta {
                id: "111.jsonl".into(),
                label: "Explique le projet".into(),
                hint: "3 msg · il y a 1 h".into(),
            },
            SessionMeta {
                id: "222.jsonl".into(),
                label: "Refactor lexer".into(),
                hint: "8 msg · il y a 2 j".into(),
            },
        ];
        s.input = "/resume ".into();
        assert!(s.menu_open());
        assert_eq!(s.menu_items().len(), 2);
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // -> 2nd session
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/resume 222.jsonl".into()));
    }

    #[test]
    fn resume_submenu_filters_or_falls_back_to_manual_id() {
        let mut s = AppState::new("gpt-5", false);
        s.sessions = vec![
            SessionMeta {
                id: "111.jsonl".into(),
                label: "Alpha".into(),
                hint: "".into(),
            },
            SessionMeta {
                id: "222.jsonl".into(),
                label: "Beta".into(),
                hint: "".into(),
            },
        ];

        s.set_input("/resume 222".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "222.jsonl");

        s.set_input("/resume missing.jsonl".into());
        assert!(!s.menu_open());
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Command("/resume missing.jsonl".into())
        );
    }

    #[test]
    fn blocks_from_messages_rebuilds_transcript() {
        let msgs = vec![
            Message::user("salut"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "voici".into(),
                },
                ContentBlock::tool_use("c1", "read", serde_json::json!({ "path": "a.rs" })),
            ]),
            Message::tool_result("c1", "contenu", false),
        ];
        let blocks = blocks_from_messages(&msgs);
        assert!(matches!(&blocks[0], Block::User(t) if t == "salut"));
        assert!(matches!(&blocks[1], Block::Assistant { text, .. } if text == "voici"));
        assert!(matches!(&blocks[2], Block::ToolCall { name, .. } if name == "read"));
        assert!(matches!(&blocks[3], Block::ToolResult { content, .. } if content == "contenu"));
    }

    #[test]
    fn models_submenu_opens_and_selection_routes_command() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // -> /models
        // Enter on a command with an argument OPENS the submenu (does not execute).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/models ");
        assert!(s.menu_open());
        assert_eq!(s.menu_items().len(), models().len());
        // The first three require code mode. Navigate to the first compatible model.
        for _ in 0..3 {
            s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/models gpt-5.5".into()));
    }

    #[test]
    fn models_submenu_refuses_a_slug_without_a_descriptor() {
        let mut s = AppState::new("gpt-5", false);
        s.set_input("/models gpt-6-preview".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "gpt-6-preview");
        assert_eq!(items[0].hint, "descriptor unavailable");
        assert!(!items[0].enabled);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn menu_items_strip_terminal_controls_from_labels_and_hints() {
        let item = MenuItem::new(
            "safe-id",
            "model\x1b]52;clipboard\x07",
            "reason\x1b[31m",
            false,
        );
        assert!(!item.label.chars().any(char::is_control));
        assert!(!item.hint.chars().any(char::is_control));
    }

    #[test]
    fn effort_submenu_opens_and_selection_routes_command() {
        let mut s = AppState::new("gpt-5.5", false);
        s.on_key(key('/'));
        let effort_idx = COMMANDS
            .iter()
            .position(|(name, _, _)| *name == "/effort")
            .unwrap();
        for _ in 0..effort_idx {
            s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/effort ");
        assert!(s.menu_open());
        assert_eq!(
            s.menu_items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh"]
        );

        s.set_input("/effort extra".into());
        let items = s.menu_items();
        assert!(items.iter().any(|item| item.id == "xhigh"));
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/effort xhigh".into()));
    }

    #[test]
    fn effort_submenu_filters_out_unsupported_values() {
        let mut s = AppState::new("gpt-5.5", false);
        s.set_input("/effort ".into());
        let ids = s
            .menu_items()
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert!(!ids.iter().any(|id| id == "none"));
        assert!(!ids.iter().any(|id| id == "minimal"));
        assert!(!ids.iter().any(|id| id == "max"));
        assert!(!ids.iter().any(|id| id == "ultra"));
    }

    #[test]
    fn effort_submenu_has_no_items_for_unknown_model() {
        let mut s = AppState::new("legacy-model", false);
        s.set_input("/effort future".into());
        let items = s.menu_items();
        assert!(items.is_empty());
    }

    #[test]
    fn effort_normalization_is_model_aware() {
        assert_eq!(
            normalize_reasoning_effort_for_model("gpt-5.5", "xhigh"),
            Some("xhigh".into())
        );
        assert_eq!(normalize_reasoning_effort_for_model("gpt-5.5", "max"), None);
        assert_eq!(
            default_reasoning_effort_for_model("gpt-5.4-mini"),
            Some("medium")
        );
    }

    #[test]
    fn tab_completes_command_name() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)); // completes /help + space
        assert_eq!(s.input, "/help ");
        assert!(
            !s.menu_open(),
            "espace présent (commande sans sous-menu) → fermé"
        );
    }

    #[test]
    fn permission_mode_routes_keys() {
        let mut s = AppState::new("gpt-5", false);
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "sensible",
            crate::diff::Diff::default(),
        ));
        // a normal keystroke does NOT type into the input during the confirmation
        assert_eq!(s.on_key(key('x')), InputAction::None);
        assert!(s.input.is_empty());
        // 'y' accepts
        assert_eq!(
            s.on_key(key('o')),
            InputAction::Permission {
                allow: true,
                remember: false
            }
        );
        assert!(s.pending.is_none());
    }

    #[test]
    fn session_scope_keys_exist_only_when_memoizable() {
        // US-009 AC1: a memoizable dialog answers with a session scope.
        let mut s = AppState::new("gpt-5", false);
        let mut prompt = PermissionPrompt::new("bash", "sensible", crate::diff::Diff::default());
        prompt.memoizable = true;
        s.pending = Some(prompt.clone());
        assert_eq!(
            s.on_key(key('a')),
            InputAction::Permission {
                allow: true,
                remember: true
            }
        );
        s.pending = Some(prompt);
        assert_eq!(
            s.on_key(key('d')),
            InputAction::Permission {
                allow: false,
                remember: true
            }
        );

        // US-009 AC2/AC5: without memoization those keys do nothing and the
        // dialog stays open.
        let mut prompt = PermissionPrompt::new("bash", "sensible", crate::diff::Diff::default());
        prompt.memo_note = Some("the command contains a substitution or a variable".into());
        s.pending = Some(prompt);
        assert_eq!(s.on_key(key('a')), InputAction::None);
        assert_eq!(s.on_key(key('d')), InputAction::None);
        assert_eq!(s.on_key(key('z')), InputAction::None);
        assert!(s.pending.is_some(), "nothing approved, dialog still open");
    }

    #[test]
    fn plain_esc_interrupts_running_turn_without_modal() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputAction::Interrupt
        );
    }

    #[test]
    fn esc_keeps_permission_priority_over_interrupt() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputAction::Permission {
                allow: false,
                remember: false
            }
        );
    }

    #[test]
    fn interrupted_event_clears_pending_and_returns_idle() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("partial".into()));
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        s.apply(&AgentEvent::Interrupted);

        assert!(s.pending.is_none());
        assert_eq!(s.status, Status::Idle);
        assert!(matches!(
            s.blocks.last(),
            Some(Block::Notice(message)) if message == "interrupted"
        ));
        assert!(matches!(
            s.blocks.first(),
            Some(Block::Assistant {
                streaming: false,
                ..
            })
        ));
    }

    #[test]
    fn first_ctrl_c_arms_quit_shortcut_second_ctrl_c_quits() {
        let mut s = AppState::new("gpt-5", false);
        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::None);
        assert!(!s.should_quit);
        assert!(s.quit_shortcut_hint_visible());

        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Quit);
        assert!(s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn shutdown_feedback_clears_modal_and_footer_hint() {
        let mut s = AppState::new("gpt-5", false);
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));
        s.arm_quit_shortcut();

        s.show_shutdown_in_progress();

        assert!(s.shutdown_in_progress());
        assert!(s.pending.is_none());
        assert_eq!(s.status, Status::Idle);
        assert!(!s.quit_shortcut_hint_visible());
        assert!(!s.is_welcome());
    }

    #[test]
    fn ctrl_c_interrupts_running_turn_before_quit() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");

        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Interrupt);
        assert!(s.quit_shortcut_hint_visible());
        assert!(!s.should_quit);

        s.apply(&AgentEvent::Interrupted);
        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn ctrl_c_keeps_permission_priority_over_interrupt() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputAction::Permission {
                allow: false,
                remember: false
            }
        );
        assert!(!s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn ctrl_c_dismisses_menu_before_quit_shortcut() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));

        assert!(s.menu_open());
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(s.input.is_empty());
        assert!(!s.menu_open());
        assert!(!s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn ctrl_t_opens_and_closes_transcript_overlay() {
        let mut s = AppState::new("gpt-5", false);
        s.input = "draft".into();
        s.cursor = s.input.len();

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(s.transcript_overlay_open());
        assert_eq!(s.input, "draft");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(!s.transcript_overlay_open());
        assert_eq!(s.input, "draft");
    }

    #[test]
    fn transcript_overlay_routes_pager_keys_without_editing_input() {
        let mut s = AppState::new("gpt-5", false);
        s.set_transcript_overlay_metrics(120, 20);
        s.open_transcript_overlay();
        s.input = "draft".into();
        s.cursor = s.input.len();

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 20);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 10);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 120);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "draft");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            InputAction::None
        );
        assert!(!s.transcript_overlay_open());
    }

    // US-044/045: life cycle of a turn's progress.
    #[test]
    fn turn_progress_lifecycle() {
        let mut s = AppState::new("gpt-5", true);
        s.begin_turn();
        assert_eq!(s.turn_chars, 0);
        assert!(s.turn_elapsed.is_none());
        s.apply(&AgentEvent::Text("abcd".into()));
        assert_eq!(s.turn_chars, 4, "chars cumulés pour l'estimation de tokens");
        s.tick_progress(std::time::Duration::from_secs(5));
        assert_eq!(s.turn_elapsed, Some(std::time::Duration::from_secs(5)));
        assert_eq!(s.spinner_tick, 1, "le tick avance l'animation");
        s.end_turn();
        assert!(
            s.turn_elapsed.is_none(),
            "indicateurs disparus en fin de tour"
        );
    }

    fn model_turn(index: u32, context_tokens: Option<u32>, window: Option<u32>) -> AgentEvent {
        AgentEvent::ModelTurn(agent_core::ModelTurnView {
            index,
            input_tokens: u64::from(index) * 1_000,
            output_tokens: u64::from(index) * 100,
            context_tokens,
            context_window: window,
            auto_compact_token_limit: window.map(|window| window * 9 / 10),
            estimated_context_tokens: None,
        })
    }

    // US-004 AC1 + AC2: the indicator is fed by the backend counters and the
    // model window, and stays absent as long as one of the two is missing. No
    // block is added to the transcript.
    #[test]
    fn context_indicator_comes_from_backend_counters() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&model_turn(1, None, None));
        assert_eq!(s.context_pct, None, "aucune mesure: indicateur absent");
        assert!(s.blocks.is_empty(), "comptabilité: aucun bloc");

        s.apply(&model_turn(2, Some(50_000), None));
        assert_eq!(s.context_pct, None, "fenêtre inconnue: toujours absent");

        s.apply(&model_turn(3, Some(50_000), Some(200_000)));
        assert_eq!(s.context_pct, Some(25));
        assert_eq!(s.total_input_tokens, 3_000);
        assert_eq!(s.total_output_tokens, 300);
    }

    // US-004 AC4: after a compaction the next round-trip reports a lower
    // occupancy, and the indicator follows it down.
    #[test]
    fn context_indicator_follows_compaction_downwards() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&model_turn(1, Some(160_000), Some(200_000)));
        assert_eq!(s.context_pct, Some(80));
        s.apply(&AgentEvent::Compacted(agent_core::CompactKind::Auto));
        s.apply(&model_turn(2, Some(20_000), Some(200_000)));
        assert_eq!(s.context_pct, Some(10), "le remplissage redescend");
    }

    // US-005: the reports name what the session does not know instead of
    // silently dropping the line.
    #[test]
    fn session_reports_name_missing_data() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "pyxis".into();
        let facts = SessionFacts {
            session_id: "20260725-101112.jsonl",
            sandbox: "enforced (workspace)",
            config_sources: &[],
            profile: None,
            runtime: RuntimeFacts::default(),
        };

        let status = session_status_report(&s, facts);
        assert!(status.contains("model: gpt-5.5"), "{status}");
        assert!(
            status.contains(&format!("reasoning effort: {UNAVAILABLE}")),
            "{status}"
        );
        assert!(status.contains("sandbox: enforced (workspace)"), "{status}");
        assert!(status.contains("workspace: pyxis"), "{status}");
        assert!(
            status.contains("session: 20260725-101112.jsonl"),
            "{status}"
        );
        assert!(status.contains("permissions: "), "{status}");

        let usage = session_usage_report(&s);
        assert!(usage.contains("input tokens: 0"), "{usage}");
        assert!(
            usage.contains(&format!("context: {UNAVAILABLE}")),
            "aucun usage rapporté: dit explicitement ({usage})"
        );
        assert!(
            usage.contains(&format!("quota: {UNAVAILABLE}")),
            "aucun quota servi: dit explicitement ({usage})"
        );
    }

    /// US-005 AC2: a value that does not come from a default names its layer, and
    /// a value nobody declared stays bare. The selected profile gets its own line:
    /// it changes several keys at once.
    #[test]
    fn session_status_names_the_layer_of_each_configured_value() {
        let mut s = AppState::new("gpt-5.6", true);
        s.reasoning_effort = Some("high".into());
        let status = session_status_report(
            &s,
            SessionFacts {
                session_id: "s.jsonl",
                sandbox: "enforced (read-only)",
                config_sources: &[
                    (SOURCE_KEY_MODEL, "command line"),
                    (SOURCE_KEY_SANDBOX_MODE, "profile"),
                ],
                profile: Some("review"),
                runtime: RuntimeFacts::default(),
            },
        );

        assert!(
            status.contains("model: gpt-5.6 (from command line)"),
            "{status}"
        );
        assert!(
            status.contains("sandbox: enforced (read-only) (from profile)"),
            "{status}"
        );
        assert!(status.contains("profile: review"), "{status}");
        // Not declared by any layer: no parenthesis invented.
        assert!(status.contains("reasoning effort: high\n"), "{status}");
        assert!(!status.contains("permissions: ask (from"), "{status}");
    }

    // US-005 AC2: once the data has arrived, consumption, fill and quota are
    // reported together.
    #[test]
    fn session_usage_reports_measures_once_known() {
        let mut s = AppState::new("gpt-5.5", true);
        s.reasoning_effort = Some("medium".into());
        s.apply(&model_turn(1, Some(50_000), Some(200_000)));
        s.apply(&AgentEvent::Quota(agent_core::quota::QuotaSnapshot {
            primary: Some(agent_core::quota::QuotaWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at_unix: Some(1_784_989_920),
            }),
            secondary: None,
        }));

        let usage = session_usage_report(&s);
        assert!(usage.contains("input tokens: 1000"), "{usage}");
        assert!(
            usage.contains("context: 50000 / 200000 tokens (25%)"),
            "{usage}"
        );
        assert!(
            usage.contains("quota: 42% used (5-hour window), resets at 2026-07-25 14:32 UTC"),
            "{usage}"
        );
        assert!(
            session_status_report(
                &s,
                SessionFacts {
                    session_id: "s.jsonl",
                    sandbox: "off (writes not restricted)",
                    config_sources: &[],
                    profile: None,
                    runtime: RuntimeFacts::default(),
                }
            )
            .contains("reasoning effort: medium")
        );
    }

    // US-046: `unseen` only counts the blocks that arrived while scrolled up, and resets
    // to zero when back at the bottom (auto-follow).
    #[test]
    fn unseen_tracks_scrolled_up_content() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("a".into()));
        s.apply(&AgentEvent::EndTurn);
        assert_eq!(s.unseen, 0, "collé en bas : rien d'unseen");
        s.scroll = 2; // the user scrolled up
        s.apply(&AgentEvent::Text("b".into())); // new block -> +1
        assert_eq!(s.unseen, 1);
        s.scroll_down(5); // back at the bottom
        assert_eq!(s.scroll, 0);
        assert_eq!(s.unseen, 0, "auto-follow → reset");
    }

    // US-046 (robustness): leaving the bottom drops a stale `unseen` (e.g. left by
    // a direct `scroll = 0` from the command path, which does not go through scroll_down).
    #[test]
    fn scroll_up_clears_stale_unseen() {
        let mut s = AppState::new("gpt-5", true);
        s.scroll_max.set(50); // scrollable content
        s.unseen = 3; // stale, while we are pinned at the bottom
        s.scroll_up(5); // we leave the bottom -> blank counter
        assert!(s.scroll > 0);
        assert_eq!(s.unseen, 0, "compteur périmé écarté en quittant le bas");
    }

    // US-046: a stream that APPENDS to the last Assistant block (without creating a new
    // block) still reports content when the user has scrolled up the transcript.
    #[test]
    fn unseen_floors_on_pure_stream_append() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("start ".into()));
        s.scroll = 2; // the user scrolls up DURING the stream
        s.apply(&AgentEvent::Text("suite".into())); // APPEND (no new block)
        assert_eq!(s.blocks.len(), 1, "un seul bloc Assistant (append)");
        assert_eq!(
            s.unseen, 1,
            "stream signals content even without a new block"
        );
    }

    // ───────────── Multi-line composer (EP-003, US-009 / US-011) ─────────────

    fn press(s: &mut AppState, code: KeyCode, modifiers: KeyModifiers) -> InputAction {
        s.on_key(KeyEvent::new(code, modifiers))
    }

    fn type_str(s: &mut AppState, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                press(s, KeyCode::Enter, KeyModifiers::ALT);
            } else {
                press(s, KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
    }

    #[test]
    fn alt_enter_ctrl_j_and_shift_enter_insert_newline_without_submitting() {
        for (code, modifiers) in [
            (KeyCode::Enter, KeyModifiers::ALT),
            (KeyCode::Enter, KeyModifiers::SHIFT),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            let mut s = AppState::new("gpt-5", false);
            type_str(&mut s, "a");
            assert_eq!(press(&mut s, code, modifiers), InputAction::None);
            type_str(&mut s, "b");
            assert_eq!(s.input, "a\nb", "{code:?} + {modifiers:?}");
            assert_eq!(s.cursor, s.input.len());
        }
    }

    #[test]
    fn plain_enter_submits_the_whole_multiline_prompt() {
        let mut s = AppState::new("gpt-5", false);
        type_str(&mut s, "ligne un\nligne deux\nligne trois");
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::Submit("ligne un\nligne deux\nligne trois".into())
        );
        assert!(s.input.is_empty());
    }

    #[test]
    fn empty_and_blank_multiline_input_submits_nothing() {
        let mut s = AppState::new("gpt-5", false);
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::None
        );
        type_str(&mut s, "\n\n");
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::None
        );
        assert_eq!(s.input, "\n\n", "une saisie blanche n'est pas effacée");
    }

    #[test]
    fn arrows_walk_lines_before_recalling_history() {
        let mut s = AppState::new("gpt-5", false);
        s.history = vec!["ancien prompt".into()];
        type_str(&mut s, "premiere\nseconde");
        // Cursor at the end of "seconde" (column 7): Up moves one line up
        // holding the column, not jumping to the end of the line.
        press(&mut s, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.input, "premiere\nseconde");
        assert_eq!(&s.input[..s.cursor], "premier");
        // Already on the first line: Up recalls the history.
        press(&mut s, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.input, "ancien prompt");
    }

    #[test]
    fn down_recalls_history_only_from_the_last_line() {
        let mut s = AppState::new("gpt-5", false);
        s.history = vec!["ancien".into()];
        press(&mut s, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.input, "ancien");
        s.set_input("un\ndeux".into());
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        press(&mut s, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.cursor, 0);
        // On the first line, Down moves down instead of recalling the history.
        press(&mut s, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(s.input, "un\ndeux");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn home_and_end_stay_on_the_current_line() {
        let mut s = AppState::new("gpt-5", false);
        type_str(&mut s, "abc\ndefgh");
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(s.cursor, 4);
        press(&mut s, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(s.cursor, s.input.len());
        press(&mut s, KeyCode::Up, KeyModifiers::NONE);
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn vertical_navigation_never_lands_inside_a_grapheme() {
        let mut s = AppState::new("gpt-5", false);
        // Line 1 narrow in cells, line 2 full of composed graphemes.
        s.set_input("漢字テスト\ne\u{301}👨\u{200d}👩\u{200d}👧 fin".into());
        for _ in 0..8 {
            press(&mut s, KeyCode::Up, KeyModifiers::NONE);
            press(&mut s, KeyCode::Down, KeyModifiers::NONE);
            press(&mut s, KeyCode::Left, KeyModifiers::NONE);
            assert!(
                s.input.is_char_boundary(s.cursor),
                "curseur {} au milieu d'un caractère",
                s.cursor
            );
        }
        while s.cursor > 0 {
            press(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
            assert!(s.input.is_char_boundary(s.cursor));
        }
    }

    #[test]
    fn multiline_input_never_opens_the_command_menu() {
        let mut s = AppState::new("gpt-5", false);
        s.sessions = vec![SessionMeta {
            id: "abc".into(),
            label: "titre".into(),
            hint: "hier".into(),
        }];
        s.set_input("/resume ".into());
        assert!(s.menu_open());
        s.insert_str("\nsuite du prompt");
        assert!(!s.menu_open());
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::Submit("/resume \nsuite du prompt".into())
        );
    }

    #[test]
    fn paste_preserves_newlines_and_does_not_submit() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_paste("fn main() {\n    println!(\"hi\");\n}");
        assert_eq!(s.input, "fn main() {\n    println!(\"hi\");\n}");
        assert_eq!(s.cursor, s.input.len());
    }

    #[test]
    fn paste_neutralizes_ansi_escape_sequences() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_paste("\u{1b}[2J\u{1b}[31mrouge\u{1b}[0m\u{7}");
        assert_eq!(s.input, "rouge");
    }

    #[test]
    fn large_paste_is_summarized_then_expanded_on_submit() {
        let mut s = AppState::new("gpt-5", false);
        let big = (0..847)
            .map(|i| format!("ligne {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.insert_paste(&big);
        assert_eq!(s.input, "[collage : 847 lignes]");
        type_str(&mut s, " analyse");
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::Submit(format!("{big} analyse")),
            "le contenu intégral part vers le modèle, jamais le résumé"
        );
        // The paste is consumed: the next submission does not replay it.
        type_str(&mut s, "[collage : 847 lignes]");
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::Submit("[collage : 847 lignes]".into())
        );
    }

    #[test]
    fn paste_at_the_summary_threshold_is_inserted_verbatim() {
        let mut s = AppState::new("gpt-5", false);
        let text = vec!["x"; PASTE_SUMMARY_MIN_LINES].join("\n");
        s.insert_paste(&text);
        assert_eq!(s.input, text);
    }

    #[test]
    fn two_large_pastes_expand_in_order() {
        let mut s = AppState::new("gpt-5", false);
        let a = vec!["a"; 600].join("\n");
        let b = vec!["b"; 700].join("\n");
        s.insert_paste(&a);
        type_str(&mut s, " puis ");
        s.insert_paste(&b);
        assert_eq!(
            s.input,
            "[collage : 600 lignes] puis [collage : 700 lignes]"
        );
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            InputAction::Submit(format!("{a} puis {b}"))
        );
    }

    // ───────── US-009: the task plan ─────────

    fn plan_event(steps: &[(&str, agent_core::PlanStatus)]) -> AgentEvent {
        AgentEvent::Plan(agent_core::PlanView {
            explanation: None,
            steps: steps
                .iter()
                .map(|(step, status)| agent_core::PlanStep {
                    step: (*step).to_string(),
                    status: *status,
                })
                .collect(),
        })
    }

    #[test]
    fn a_plan_enters_the_transcript_as_one_block() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&plan_event(&[
            ("lire", agent_core::PlanStatus::Completed),
            ("écrire", agent_core::PlanStatus::InProgress),
        ]));
        let plans: Vec<&Block> = s
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Plan(_)))
            .collect();
        assert_eq!(plans.len(), 1);
        match plans[0] {
            Block::Plan(view) => {
                assert_eq!(view.steps.len(), 2);
                assert_eq!(view.steps[1].status, agent_core::PlanStatus::InProgress);
            }
            other => unreachable!("expected a plan block, got {other:?}"),
        }
    }

    #[test]
    fn an_updated_plan_replaces_the_previous_one() {
        // US-009 AC4: the display reflects the new state WITHOUT stacking the
        // old one; the reader sees one plan, and it is the current one.
        let mut s = AppState::new("gpt-5", false);
        s.apply(&plan_event(&[("lire", agent_core::PlanStatus::InProgress)]));
        s.apply(&AgentEvent::Text("j'avance".into()));
        s.apply(&plan_event(&[
            ("lire", agent_core::PlanStatus::Completed),
            ("écrire", agent_core::PlanStatus::InProgress),
        ]));

        let plans: Vec<&Block> = s
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Plan(_)))
            .collect();
        assert_eq!(plans.len(), 1, "un seul plan doit rester: {:?}", s.blocks);
        match plans[0] {
            Block::Plan(view) => assert_eq!(view.steps.len(), 2, "c'est le plan à jour"),
            other => unreachable!("expected a plan block, got {other:?}"),
        }
        assert!(
            matches!(s.blocks.last(), Some(Block::Plan(_))),
            "le plan à jour se place au point courant de la conversation"
        );
    }
}
