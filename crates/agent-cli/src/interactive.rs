//! Interactive loop: assembles the frontend (`agent-tui`), the thread runtime
//! (`agent-runtime`) and the permission requests into a single `tokio::select`.
//!
//! - Keystrokes arrive from a dedicated thread (crossterm `read()` blocks).
//! - Every input is an operation SUBMITTED to a `ThreadHandle`; the turn
//!   lifecycle, the ordering, the steering and the interruption belong to the
//!   runtime. This loop owns none of them, which is what US-017 removed: no
//!   client-side turn object, no turn counter, no direct join handle and no
//!   post-turn prompt FIFO. A test at the bottom of this file keeps them out.
//! - A permission request suspends the tool pipeline until the user
//!   answers (the dialog does NOT freeze the loop: the select keeps rendering
//!   and reading the keyboard).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_core::message::{ContentBlock, Message, recent_untrusted_content};
use agent_core::provider::Provider;
use agent_core::{AgentEvent, Session};
use agent_provider::KEYRING_ACCOUNT;
use agent_runtime::lifecycle::TurnState;
use agent_runtime::thread::{
    RuntimeEvent, RuntimeEventPayload, Submission, SubmitError, ThreadStatus,
};
use agent_tools::PermissionModeState;
use agent_tui::{
    AppState, Block, COMMANDS, InputAction, McpServerMeta, McpStatus, SessionMeta,
    blocks_from_messages, default_reasoning_effort_for_model, normalize_reasoning_effort_for_model,
    permission_mode_label, reasoning_effort_label, supported_reasoning_efforts_for_model,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::runtime::{CliStepSource, EngineDeps, SessionRuntime, SettingsCell};
#[cfg(feature = "codex_tui_parity")]
use agent_tui::{
    ChatWidget, HistoryInserter, InsertHistoryMode, PermissionTranscriptRequest, TerminalViewport,
    TerminalViewportState,
};
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use tokio::sync::{mpsc, oneshot};

use crate::approver::{PermissionMsg, to_prompt};
use crate::settings::{permission_mode_from_arg, permission_mode_id};

/// Maximum number of prompt history entries aggregated per directory.
const PROMPT_HISTORY_CAP: usize = 200;

/// Result of an MCP connection started in the background. Comes back into the
/// `select!` loop to update the registry and the display without freezing the TUI.
enum McpEvent {
    Connected {
        name: String,
        conn: agent_mcp::McpConnection,
        tools: Vec<agent_mcp::McpToolInfo>,
    },
    Failed {
        name: String,
        error: String,
    },
}

pub struct InteractiveConfig {
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Tool registry, shared with the loop that dispatches through it. Held here
    /// because the exposed set moves in session (US-016): the specs are read from
    /// it at each step boundary, never frozen at startup.
    pub registry: Arc<agent_tools::Registry>,
    pub truecolor: bool,
    /// Reduced motion (`NO_COLOR` / `PYXIS_REDUCED_MOTION`): spinner degraded to a
    /// pulsing dot rather than animated (US-044).
    pub reduced_motion: bool,
    /// Provider credential present (connected badge + providers submenu).
    pub connected: bool,
    /// Skills installed on the machine (US-014): `/skills` submenu, catalog
    /// exposed to the model, and body injected when one is invoked.
    pub skills: crate::skills::Catalog,
    /// Persistent session goal (`/goal`), composed into the system prompt on every
    /// turn. Loaded from the session sidecar at startup.
    pub goal: Option<String>,
    /// Lifecycle hooks (US-017). Shared with the tool registry, which uses the
    /// same engine for `PreToolUse` and `PostToolUse`.
    pub hooks: Arc<dyn agent_tools::hooks::Hooks>,
    /// The same hooks as declared, kept for `/hooks` (US-019 AC6). The engine
    /// above answers "does anything watch this?", not "what is installed?".
    pub hook_specs: Vec<agent_tools::hooks::HookSpec>,
    /// Hardening applied to the MCP subprocesses (env scrub + proxy).
    pub command_hardener: agent_tools::CommandHardener,
    /// Diagnostics of the MCP startup connection (US-012): unavailable server,
    /// server left behind the trust gate, tool not exposable. Successes are silent,
    /// the `/mcp` submenu already shows them.
    pub mcp_notices: Vec<String>,
    /// Model-facing tool names exposed per MCP server (US-016). A disconnect takes
    /// exactly these names back out, and the union is what keeps a name handed out
    /// once from ever being handed out twice.
    pub mcp_tool_names: BTreeMap<String, BTreeSet<String>>,
    /// Sub-agent wiring: the spawner and the handle the six multi-agent tools
    /// address. `None` when the build has no spawner.
    pub agents: Option<crate::runtime::AgentWiring>,
    /// Mutable permission mode, shared with the tool registry.
    pub permission_mode: PermissionModeState,
    /// Answers remembered this session, shared with the tool registry
    /// (US-009 inspection surface). In memory only, never persisted.
    pub approvals: agent_tools::permission::ApprovalMemory,
    /// Global user settings, used to persist the interactive choices.
    pub settings_path: Option<PathBuf>,
    /// Workspace root, scope of the aggregated turn diff (US-018).
    pub workspace: PathBuf,
    /// Sandbox scope as the binary resolved it, displayed by `/status`
    /// (US-005). Resolved there because enforcement happens before this loop
    /// exists.
    pub sandbox_scope: String,
    /// Configuration layer each displayed value comes from (US-005 AC2), in the
    /// `agent_tui::SOURCE_KEY_*` vocabulary.
    pub config_sources: Vec<(&'static str, &'static str)>,
    /// Profile applied to this session (US-006), shown by `/status`.
    pub profile: Option<String>,
    /// What a thread needs to run a turn, minus the session: the runtime opens
    /// that itself (EP-005).
    pub engine: EngineDeps,
    /// Configuration every turn is CAPTURED from. The loop writes its session
    /// changes here; the runtime reads it once per turn.
    pub settings: Arc<SettingsCell>,
    /// What the model sees at each step: tool catalog, project context, invoked
    /// skill bodies.
    pub steps: Arc<CliStepSource>,
    /// Kept out of `EngineDeps` reach for the two things the loop does with it
    /// directly: the prompt cache key of a session, and signing out.
    pub provider: Arc<dyn Provider>,
}

/// US-005 AC2: provenance is stated only for the values still as the
/// configuration resolved them. A layer that no longer explains the displayed
/// value would be worse than no layer at all: it would be a wrong answer to
/// "where does this come from?".
fn config_sources_still_valid(
    cfg: &InteractiveConfig,
    resolved_model: &str,
    resolved_effort: &Option<String>,
    resolved_permission_mode: agent_tools::permission::PermissionMode,
) -> Vec<(&'static str, &'static str)> {
    let mut sources = cfg.config_sources.clone();
    let mut drop_key = |key: &str| sources.retain(|(owned, _)| *owned != key);
    if cfg.model != resolved_model {
        drop_key(agent_tui::SOURCE_KEY_MODEL);
    }
    if cfg.reasoning_effort != *resolved_effort {
        drop_key(agent_tui::SOURCE_KEY_REASONING_EFFORT);
    }
    if cfg.permission_mode.get() != resolved_permission_mode {
        drop_key(agent_tui::SOURCE_KEY_PERMISSION_MODE);
    }
    sources
}

/// US-006: one line per modified file, in the same scope as the aggregated turn
/// diff. Bounded on purpose: dumping the unified diffs into the transcript
/// would flood the view for a command whose job is to say what changed.
fn workspace_diff_report(diff: &agent_core::TurnDiffView) -> String {
    if diff.is_empty() {
        return "No change in the workspace.".to_string();
    }
    let mut report = String::from(agent_tui::turn_diff_summary(diff).as_str());
    for file in &diff.files {
        let mark = match file.change {
            agent_core::FileChange::Added => 'A',
            agent_core::FileChange::Modified => 'M',
            agent_core::FileChange::Deleted => 'D',
        };
        report.push_str(&format!(
            "\n  {mark} {} +{} -{}",
            file.path, file.added_lines, file.removed_lines
        ));
    }
    report
}

/// US-009 AC3: what the session remembers, and how to forget it. The token
/// sequences are shown as they were approved, so the user can check that no
/// answer covers more than what was answered.
fn approvals_report(entries: &[agent_tools::permission::ApprovalEntry]) -> String {
    if entries.is_empty() {
        return "No answer remembered this session. They are never persisted to disk.".to_string();
    }
    let mut report = format!("Remembered answers ({}):", entries.len());
    for entry in entries {
        let verdict = if entry.allow { "allow" } else { "deny" };
        report.push_str(&format!("\n  {verdict}  {} {}", entry.tool, entry.command));
    }
    report.push_str("\nUse /approvals clear to forget them.");
    report
}

/// Completion marker emitted by the model when the goal is fully
/// reached. Detected by the harness to auto-clear the goal; stripped from the display.
pub const GOAL_DONE_MARKER: &str = "<<GOAL_DONE>>";

/// Guardrail: max number of automatic re-prompts per goal (anti-runaway).
const MAX_GOAL_ITERS: u32 = 25;

/// Message injected on every automatic re-prompt as long as the goal is not marked reached.
const GOAL_CONTINUE_PROMPT: &str = "Continue the session goal. If work remains, \
    keep going. If it is fully completed and verified, end your reply with \
    <<GOAL_DONE>> alone on the final line.";

/// Granularity of the keyboard reader. Short enough to stay imperceptible while
/// typing, long enough not to monopolize the crossterm event queue
/// while another call reads the terminal answer from it.
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(25);

fn persist_model(state: &mut AppState, settings_path: Option<&Path>, model: &str) {
    if let Some(path) = settings_path
        && let Err(err) = crate::settings::save_model(path, model)
    {
        state.blocks.push(Block::Error(format!(
            "settings: failed to save model: {err}"
        )));
    }
}

fn persist_reasoning_effort(
    state: &mut AppState,
    settings_path: Option<&Path>,
    effort: Option<&str>,
) {
    if let Some(path) = settings_path
        && let Err(err) = crate::settings::save_reasoning_effort(path, effort)
    {
        state.blocks.push(Block::Error(format!(
            "settings: failed to save reasoning effort: {err}"
        )));
    }
}

/// Composes the effective system prompt: base + completion DIRECTIVE. The goal
/// lives in `instructions` (re-sent every turn) so it survives compaction:
/// `agent-core::compaction` only touches `messages`, never the system prompt.
pub fn compose_system(base: &str, goal: Option<&str>) -> String {
    match goal {
        Some(g) if !g.trim().is_empty() => format!(
            "{base}\n\n\
             ## Session Goal: DO NOT STOP before it is FULLY completed\n\
             {g}\n\n\
             Work continuously (read, edit, execute) until this goal is ENTIRELY \
             complete, without asking for confirmation. As long as anything remains, \
             continue. When, and only when, the goal is fully completed and verified, \
             end your final reply with the exact marker alone on its final line:\n{GOAL_DONE_MARKER}\n\
             NEVER write this marker until the goal is fully completed."
        ),
        _ => base.to_string(),
    }
}

/// Injects the behavioral guidelines of the tools (US-026) into the system
/// prompt under a dedicated section. Called ONCE at startup (the tools are
/// fixed) to produce the base that `compose_system` then enriches per turn.
/// Without a guideline, returns the base unchanged (no empty section).
pub fn with_tool_guidelines(base: &str, guidelines: &[String]) -> String {
    if guidelines.is_empty() {
        return base.to_string();
    }
    let mut s = String::from(base);
    s.push_str("\n\n## Tool Usage Rules\n");
    for g in guidelines {
        s.push_str("- ");
        s.push_str(g);
        s.push('\n');
    }
    s.truncate(s.trim_end().len());
    s
}

pub(crate) fn prompt_cache_key_for_session(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("session");
    format!("pyxis-{stem}")
}

pub(crate) fn goal_path_for_session(path: &Path) -> PathBuf {
    path.with_extension("goal")
}

pub(crate) fn goal_iters_path_for_session(path: &Path) -> PathBuf {
    path.with_extension("goal.iters")
}

pub(crate) fn read_goal(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Instruction block injected by `/init` (US-019 AC1). Ephemeral for the turn,
/// like a skill body: the transcript keeps the `/init` the user typed, which is
/// what says afterwards where the file came from. It asks for a REAL inspection
/// because an AGENTS.md written from the model's priors would describe a
/// plausible repository rather than this one.
const INIT_PROMPT: &str = "\
Bootstrap the contributor instructions of this repository.

1. Inspect the repository for real: list the root, read the build manifests, \
locate the sources, the tests and the CI configuration, and read enough of them \
to describe what is actually there.
2. Write `AGENTS.md` at the root of the workspace with only what a new \
contributor cannot guess: the build, test and lint commands that really work \
here, the layout of the code, the conventions the existing code follows, and \
the invariants that must not be broken.
3. Keep it short and factual. Never state a command you have not seen declared \
somewhere in the repository, and do not restate what the file tree already says.

Write the file with your file-writing tool, then answer with a one-line summary \
of what you put in it.";

/// What `/init` does about an instruction file that is already there (AC2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitDecision {
    /// Nothing to protect: the bootstrap turn starts.
    Bootstrap,
    /// A file exists and `force` was not typed: the command refuses and names it.
    Confirm(&'static str),
    /// A file exists and the user confirmed by typing `/init force`.
    Overwrite(&'static str),
}

/// `/init` never overwrites an instruction file on its own: the confirmation is
/// the explicit `force` argument, decided BEFORE the turn starts. Leaving the
/// guard to the permission pipeline would not do, because `accept-edits` and
/// `auto` approve a write without asking anyone.
fn init_decision(workspace: &Path, arg: &str) -> InitDecision {
    let forced = arg.trim() == "force";
    match crate::context::instructions_file(workspace) {
        Some(name) if forced => InitDecision::Overwrite(name),
        Some(name) => InitDecision::Confirm(name),
        None => InitDecision::Bootstrap,
    }
}

/// Clipboard helpers tried in order (`/copy`, US-019 AC4). Declared as an argv
/// and not as a command line: nothing is re-interpreted by a shell between this
/// table and the process. The first one present on the machine wins.
#[cfg(target_os = "macos")]
const CLIPBOARD_HELPERS: &[(&str, &[&str])] = &[("pbcopy", &[])];
#[cfg(not(target_os = "macos"))]
const CLIPBOARD_HELPERS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Raw text of the last assistant answer, as it was streamed (US-019 AC4): no
/// rendering, no markup added, and the goal marker removed like the headless
/// output does. `None` when no answer has been displayed yet.
fn last_assistant_text(state: &AppState) -> Option<String> {
    state.blocks.iter().rev().find_map(|block| match block {
        Block::Assistant { text, .. } => {
            Some(text.replace(GOAL_DONE_MARKER, "").trim_end().to_string())
        }
        _ => None,
    })
}

/// Names the clipboard failure instead of letting `/copy` claim a copy that did
/// not happen (AC4). Lists what was tried, because "no clipboard" almost always
/// means "the helper for this session type is not installed".
fn clipboard_failure(errors: &[String]) -> String {
    let tried = CLIPBOARD_HELPERS
        .iter()
        .map(|(program, _)| *program)
        .collect::<Vec<_>>()
        .join(", ");
    format!("no usable clipboard helper (tried: {tried}) — {}", {
        if errors.is_empty() {
            "none of them is installed".to_string()
        } else {
            errors.join("; ")
        }
    })
}

/// Ceiling on a clipboard helper. They all fork into the background, so the
/// foreground process returns at once; a helper that does not is a hang of the
/// WHOLE interface, since this runs inside the event loop.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(3);

/// Writes `text` to the system clipboard and returns the helper that took it.
async fn copy_to_clipboard(text: &str) -> Result<&'static str, String> {
    use tokio::io::AsyncWriteExt;

    let mut errors: Vec<String> = Vec::new();
    for (program, args) in CLIPBOARD_HELPERS {
        let mut child = match tokio::process::Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            // Not installed on this machine: try the next one, and keep the
            // reason in case none of them works.
            Err(err) => {
                errors.push(format!("{program}: {err}"));
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(err) = stdin.write_all(text.as_bytes()).await {
                errors.push(format!("{program}: {err}"));
                let _ = child.kill().await;
                continue;
            }
            // Dropped BEFORE the wait: the helper reads until EOF.
            drop(stdin);
        }
        match tokio::time::timeout(CLIPBOARD_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) if status.success() => return Ok(program),
            Ok(Ok(status)) => errors.push(format!("{program}: {status}")),
            Ok(Err(err)) => errors.push(format!("{program}: {err}")),
            Err(_) => {
                errors.push(format!(
                    "{program}: no answer after {}s",
                    CLIPBOARD_TIMEOUT.as_secs()
                ));
                let _ = child.kill().await;
            }
        }
    }
    Err(clipboard_failure(&errors))
}

/// What `/logout` can promise and what it cannot (US-019 AC5). Nothing here
/// reaches OpenAI: the credential is deleted locally, so the ChatGPT session
/// itself stays open until the user revokes it from their account.
const LOGOUT_SERVER_NOTE: &str = "The ChatGPT session is NOT revoked server-side: \
     only the local credential is deleted. Revoke it from your OpenAI account to \
     close it everywhere.";

/// Local sign-out, shared by `/logout` and `/providers subscription codex
/// disconnect` so the two cannot drift apart. Keyring first: if the provider
/// forgot the credential while the stored one survived, the next start would
/// silently reconnect.
async fn sign_out(provider: &Arc<dyn Provider>) -> Result<(), String> {
    agent_auth::store::delete(KEYRING_ACCOUNT).map_err(|err| format!("keyring: {err}"))?;
    provider
        .disconnect_auth()
        .await
        .map_err(|err| format!("provider: {err}"))
}

/// `/hooks`: what is declared, on which event, with which matcher (US-019 AC6).
/// The argv is shown as declared, since a hook is an argv and not a command line.
fn hooks_report(specs: &[agent_tools::hooks::HookSpec]) -> String {
    if specs.is_empty() {
        return "No hook declared. `hooks` is a global settings key: a workspace file \
                cannot declare one."
            .to_string();
    }
    let mut lines = vec![format!("{} hook(s) declared:", specs.len())];
    for spec in specs {
        let argv = std::iter::once(spec.command.as_str())
            .chain(spec.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        // `*` = every tool, `-` = this event watches the session and names no
        // tool at all. Showing `*` on a lifecycle event would read as "all
        // tools", which is not the same statement.
        let matcher = match spec.matcher.as_deref() {
            Some(tool) => tool,
            None if spec.event.is_tool_scoped() => "*",
            None => "-",
        };
        lines.push(format!(
            "  {:<16} matcher={matcher:<10} {argv}",
            spec.event.name(),
        ));
    }
    lines.join("\n")
}

/// When the last assistant reply carries the completion marker, removes it
/// from the display and returns `true` (goal reached).
fn take_goal_done(state: &mut AppState) -> bool {
    for block in state.blocks.iter_mut().rev() {
        if let Block::Assistant { text, .. } = block {
            let trimmed = text.trim_end();
            let marker_is_last_line = trimmed
                .lines()
                .next_back()
                .is_some_and(|line| line.trim() == GOAL_DONE_MARKER);
            if marker_is_last_line {
                let mut lines: Vec<&str> = trimmed.lines().collect();
                if lines
                    .last()
                    .is_some_and(|line| line.trim() == GOAL_DONE_MARKER)
                {
                    lines.pop();
                }
                *text = lines.join("\n").trim_end().to_string();
                return true;
            }
            return false;
        }
    }
    false
}

pub(crate) fn session_path_from_arg(sessions_dir: &Path, arg: &str) -> Option<PathBuf> {
    let candidate = Path::new(arg);
    if arg.trim().is_empty()
        || candidate.components().count() != 1
        || candidate.extension().and_then(|e| e.to_str()) != Some("jsonl")
    {
        return None;
    }
    Some(sessions_dir.join(candidate))
}

fn read_goal_iters(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

fn write_goal_iters(path: &Path, value: u32) -> std::io::Result<()> {
    std::fs::write(path, value.to_string())
}

/// Quit path. The turn is NOT killed here: the runtime's shutdown closes
/// admission, cancels the tree, drains the tasks and writes the terminal the
/// turn owes. All this does is stop waiting on a human and let the loop exit.
fn show_shutdown_feedback(
    state: &mut AppState,
    pending_resp: &mut Option<oneshot::Sender<agent_tools::permission::ApprovalResponse>>,
    turn_start: &mut Option<Instant>,
) {
    if let Some(resp) = pending_resp.take() {
        let _ = resp.send(agent_tools::permission::ApprovalResponse::DENY_ONCE);
    }
    *turn_start = None;
    state.end_turn();
    state.show_shutdown_in_progress();
    state.should_quit = true;
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

fn scrub_encrypted_reasoning(messages: &mut [Message]) -> usize {
    let mut removed = 0usize;
    for msg in messages {
        let before = msg.content.len();
        msg.content
            .retain(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }));
        removed += before.saturating_sub(msg.content.len());
    }
    removed
}

fn count_encrypted_reasoning(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|msg| {
            msg.content
                .iter()
                .filter(|b| matches!(b, ContentBlock::EncryptedReasoning { .. }))
                .count()
        })
        .sum()
}

/// True while the thread is running a turn. Read from the runtime's last-state
/// signal, never inferred from the events the loop happened to see.
fn is_running(status: &ThreadStatus) -> bool {
    matches!(&status.health, agent_runtime::ThreadHealth::Healthy)
        && status.turn.is_some_and(|turn| !turn.state.is_terminal())
}

/// Mirrors the runtime's last state into the frontend (US-017 AC5). Thread,
/// turn, state and queue depth are displayed from HERE: the TUI never re-reads
/// the store to answer "where am I?".
fn apply_runtime_status(state: &mut AppState, status: &ThreadStatus) {
    state.thread_id = status.thread_id.to_string();
    state.turn_id = status.turn.map(|turn| turn.turn_id.to_string());
    state.turn_state = status.turn.map(|turn| turn.state.as_str().to_string());
    state.pending_inputs = status.pending_inputs.saturating_add(status.pending_steers);
}

/// The v1 orchestration bounds, as `/status` reports them (US-019 AC3).
///
/// Read from the runtime CONSTANTS, never from a setting: FR-20 forbids a
/// configuration key for orchestration in v1, and a `/status` that read one
/// would be describing a knob that does not exist.
fn runtime_facts() -> agent_tui::RuntimeFacts {
    agent_tui::RuntimeFacts {
        // EP-004 built the supervisor and its five tools but the binary does not
        // expose them yet, so a session of this version owns no child. Reported
        // as the zero it is, next to the bounds that would apply.
        active_agents: 0,
        max_active_agents: agent_runtime::MAX_ACTIVE_AGENTS,
        max_agents_per_root: agent_runtime::MAX_AGENTS_PER_ROOT,
        max_agent_depth: agent_runtime::MAX_AGENT_DEPTH,
        command_mailbox: agent_runtime::COMMAND_MAILBOX,
        max_pending_inputs: agent_runtime::MAX_PENDING_INPUTS,
    }
}

/// Reads the optional `<turn-id>` argument of `/fork` and `/rewind`.
///
/// An identifier that does not parse is refused HERE, before the runtime is
/// asked for anything: a malformed argument is a typo, not a branch that failed.
fn parse_turn_argument(arg: &str) -> Result<Option<agent_runtime::TurnId>, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(None);
    }
    agent_runtime::TurnId::parse(arg)
        .map(Some)
        .map_err(|err| format!("turn id `{arg}`: {err}"))
}

/// A conversation the loop is driving.
struct OpenSession {
    runtime: SessionRuntime,
    events: broadcast::Receiver<RuntimeEvent>,
    status: ThreadStatus,
}

/// Opens the thread whose durable log is `path` and binds the provider's prompt
/// cache key to it.
async fn open_session(
    cfg: &InteractiveConfig,
    path: &Path,
    root: &CancellationToken,
) -> anyhow::Result<OpenSession> {
    let runtime = SessionRuntime::open(
        Some(path),
        cfg.engine.clone(),
        Arc::clone(&cfg.registry),
        Arc::clone(&cfg.settings),
        Arc::clone(&cfg.steps),
        root,
        cfg.agents.as_ref(),
    )
    .await?;
    // Every way into a thread goes through here, `/fork` and `/rewind`
    // included, so this is the single place a Code Mode session is attached and
    // the previous one closed (US-009 AC3).
    cfg.steps.bind_thread(&runtime.thread_id()).await;
    cfg.provider
        .set_prompt_cache_key(&prompt_cache_key_for_session(path));
    let events = runtime.subscribe();
    let status = runtime.status();
    Ok(OpenSession {
        runtime,
        events,
        status,
    })
}

/// Pushes the session settings the loop owns into the cell the runtime captures
/// each turn from. Called after every command that moves one of them, so the
/// NEXT turn is captured from what the user last asked for.
fn sync_settings(cfg: &InteractiveConfig) {
    cfg.settings.update(|settings| {
        settings.model = cfg.model.clone();
        settings.reasoning_effort = cfg.reasoning_effort.clone();
        settings.goal = cfg.goal.clone();
        settings.permission_mode = permission_mode_id(cfg.permission_mode.get()).to_string();
    });
}

/// Starts the interactive session. Restores the terminal on exit whatever
/// happens, including when the runtime channel closes under it (US-017 AC8).
pub async fn run(
    perm_rx: mpsc::Receiver<PermissionMsg>,
    hook_notices: mpsc::Receiver<String>,
    cfg: InteractiveConfig,
    sessions_dir: PathBuf,
    current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
) -> anyhow::Result<()> {
    // Root of the process cancellation tree. Every thread the session opens is a
    // CHILD of it, so quitting reaches every turn, tool and process descendant.
    let root = CancellationToken::new();
    let mut tui = agent_tui::enter()?;
    let result = event_loop(
        &mut tui,
        perm_rx,
        hook_notices,
        cfg,
        sessions_dir,
        current_session,
        mcp,
        &root,
    )
    .await;
    let clear_result = agent_tui::clear(&mut tui);
    agent_tui::leave(&mut tui)?;
    // Whatever happened above, nothing of this session is left running.
    root.cancel();
    clear_result?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    tui: &mut agent_tui::Tui,
    mut perm_rx: mpsc::Receiver<PermissionMsg>,
    mut hook_notices: mpsc::Receiver<String>,
    mut cfg: InteractiveConfig,
    sessions_dir: PathBuf,
    mut current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
    root: &CancellationToken,
) -> anyhow::Result<()> {
    // US-005 AC2: the values as the CONFIGURATION resolved them. `/models`,
    // `/effort` and `/permissions` change them in session, and a layer name would
    // then describe a value that is no longer the one displayed.
    let resolved_model = cfg.model.clone();
    let resolved_effort = cfg.reasoning_effort.clone();
    let resolved_permission_mode = cfg.permission_mode.get();

    let mut state = AppState::new(cfg.model.clone(), cfg.truecolor);
    state.set_permission_mode(permission_mode_id(cfg.permission_mode.get()));
    state.workspace = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    state.reasoning_effort = cfg.reasoning_effort.clone();
    state.provider_connected = cfg.connected;
    state.reduced_motion = cfg.reduced_motion;
    state.skills = cfg.skills.names();
    state.files = std::env::current_dir()
        .ok()
        .map(|root| workspace_file_mentions(&root, 200))
        .unwrap_or_default();
    state.sessions = load_sessions(&sessions_dir, &current_session);
    state.mcp_servers = mcp_metas(&mcp);
    // US-012: only the problems are shown. A silent startup keeps the welcome
    // screen; a server left out is worth losing it.
    for notice in std::mem::take(&mut cfg.mcp_notices) {
        state.blocks.push(Block::Notice(notice));
    }
    // US-016: names handed out per server. Taken out of the config because they
    // change with every connect and disconnect of the session.
    let mut mcp_tool_names = std::mem::take(&mut cfg.mcp_tool_names);
    let registry = Arc::clone(&cfg.registry);
    let mut goal_path = goal_path_for_session(&current_session);
    let mut goal_iters_path = goal_iters_path_for_session(&current_session);
    // Prompt history of the WHOLE directory (every conversation).
    state.load_history(agent_session::workspace_prompts(
        &sessions_dir,
        Some(&current_session),
        PROMPT_HISTORY_CAP,
    ));
    // The thread runtime of the CURRENT conversation. `/new`, `/resume`, `/fork`
    // and `/rewind` replace it wholesale rather than moving a file under a live
    // writer: a conversation is a thread, and switching conversation is opening
    // another one.
    let OpenSession {
        mut runtime,
        mut events,
        mut status,
    } = open_session(&cfg, &current_session, root).await?;
    let initial_messages = runtime.messages();
    if !initial_messages.is_empty() {
        state.blocks = blocks_from_messages(&initial_messages);
        state.blocks.push(Block::Notice(format!(
            "Session resumed ({} messages).",
            initial_messages.len()
        )));
    }
    apply_runtime_status(&mut state, &status);
    #[cfg(feature = "codex_tui_parity")]
    let mut chat = ChatWidget::new(&initial_messages);
    #[cfg(feature = "codex_tui_parity")]
    let mut viewport_sync_enabled = true;
    #[cfg(feature = "codex_tui_parity")]
    let mut last_logged_geometry: Option<String> = None;
    let mut parity_inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);
    #[cfg(feature = "codex_tui_parity")]
    let mut parity_viewport = TerminalViewportState::new(
        TerminalViewport::new(1, 1, 1),
        InsertHistoryMode::InlineScrollback,
    );
    // Empty transcript at startup -> the welcome screen (card + logo) shows
    // by itself (see `AppState::is_welcome`), no Notice to push.

    // Keyboard reader thread -> mpsc. `poll` then `read` (never `read` alone):
    // a blocking `read` holds the INTERNAL crossterm events (including the
    // answer to the cursor position request) in its local buffer until the
    // next keystroke. Yet ratatui queries the cursor on every resize
    // of the inline viewport: the answer never arrived, the request timed out, and the
    // `draw` surfaced a fatal error. `poll` files those events in the shared
    // internal queue, where `cursor::position()` finds them again.
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || {
        loop {
            match crossterm::event::poll(KEY_POLL_INTERVAL) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let Ok(ev) = crossterm::event::read() else {
                break;
            };
            if key_tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });

    let (mcp_tx, mut mcp_rx) = mpsc::channel::<McpEvent>(16);
    let mut running = is_running(&status);
    // Counter of automatic re-prompts of the goal loop (reset on every
    // user input / new goal).
    let mut goal_iters: u32 = if cfg.goal.is_some() {
        read_goal_iters(&goal_iters_path)
    } else {
        0
    };
    // US-019: set by `/init`, consumed when the turn it started ends. The project
    // context is read once, before the turn; only a re-read makes a file written
    // DURING that turn count for the next one.
    let mut refresh_context = false;
    let mut pending_resp: Option<oneshot::Sender<agent_tools::permission::ApprovalResponse>> = None;
    // US-019: a hook running after a tool call reports its failures here. The
    // branch closes for good once the emitter is gone (no hook declared, or
    // registry dropped), so the loop never spins on a closed channel.
    let mut hook_notices_open = true;
    // Aggregated workspace diff of the running turn. Opened when a turn starts,
    // read when it reaches its terminal.
    let mut diff_tracker: Option<agent_tools::turn_diff::TurnDiffTracker> = None;
    // Set when the loop must stop because the runtime went away (AC8).
    let mut runtime_failure: Option<String> = None;

    // Spinner animation tick (US-044). 100 ms is about 10 fps: fluid and nearly free
    // (the render cache serves the baked blocks). `Skip` avoids any redraw burst when
    // coming back from idle. The `select!` branch is guarded by `if running` -> 0 CPU when idle.
    let mut spinner = tokio::time::interval(Duration::from_millis(100));
    spinner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Start of the current turn (rising edge of `running`) for the elapsed time.
    let mut turn_start: Option<Instant> = None;

    loop {
        // Rising/falling edge of `running`: starts / freezes the progress
        // tracking (spinner, duration, tokens). Does NOT alter the orchestration.
        match (running, turn_start.is_some()) {
            (true, false) => {
                turn_start = Some(Instant::now());
                state.begin_turn();
            }
            (false, true) => {
                turn_start = None;
                state.end_turn();
            }
            _ => {}
        }
        #[cfg(feature = "codex_tui_parity")]
        {
            chat.sync_local_blocks(&state);
            // Terminal enlarged: the inline viewport keeps its original height and
            // leaves the composer anchored in the middle of the screen until it is
            // rebuilt. A failure (terminal not answering the position
            // request) must not kill the session: we give up the realignment
            // and go on with the stale viewport.
            if viewport_sync_enabled && let Err(err) = agent_tui::sync_inline_viewport(tui) {
                viewport_sync_enabled = false;
                agent_tui::debug_log::log(&format!("sync: disabled after error: {err}"));
                state.blocks.push(Block::Notice(format!(
                    "Viewport resize disabled: {err}. Restart Pyxis after resizing."
                )));
            }
            let size = tui.size()?;
            if agent_tui::debug_log::enabled() {
                let viewport = tui.get_frame().area();
                let line = format!(
                    "frame: screen={}x{} viewport=(x{} y{} w{} h{}) sync_enabled={viewport_sync_enabled}",
                    size.width,
                    size.height,
                    viewport.x,
                    viewport.y,
                    viewport.width,
                    viewport.height
                );
                if last_logged_geometry.as_deref() != Some(line.as_str()) {
                    agent_tui::debug_log::log(&line);
                    last_logged_geometry = Some(line);
                }
            }
            parity_viewport.resize(size.width, size.height, size.height);
            if parity_inserter.mode() == InsertHistoryMode::InlineScrollback
                && let Some(insert) = chat
                    .surface_mut()
                    .drain_pending_insert(size.width, parity_inserter.mode())
                && let Err(err) = parity_inserter.insert(tui, &insert)
            {
                parity_viewport.activate_legacy_fallback(err.message().to_string());
                state.blocks.push(Block::Notice(err.message().to_string()));
            }
        }
        #[cfg(feature = "codex_tui_parity")]
        {
            if parity_inserter.mode() == InsertHistoryMode::InlineScrollback {
                tui.draw(|frame| chat.render(frame, &state))?;
            } else {
                tui.draw(|f| agent_tui::render(f, &state))?;
            }
        }
        #[cfg(not(feature = "codex_tui_parity"))]
        tui.draw(|f| agent_tui::render(f, &state))?;
        if state.should_quit {
            break;
        }

        tokio::select! {
            ev = key_rx.recv() => {
                let k = match ev {
                    None => break, // event channel closed -> we exit
                    Some(Event::Mouse(m)) => {
                        // wheel -> transcript scroll (mouse capture enabled).
                        match m.kind {
                            MouseEventKind::ScrollUp => state.scroll_up(3),
                            MouseEventKind::ScrollDown => state.scroll_down(3),
                            _ => {}
                        }
                        continue;
                    }
                    Some(Event::Paste(p)) => {
                        #[cfg(feature = "codex_tui_parity")]
                        {
                            chat.route_paste(&mut state, &p);
                        }
                        #[cfg(not(feature = "codex_tui_parity"))]
                        {
                            if state.pending.is_none() {
                                state.insert_paste(&p);
                            }
                        }
                        continue;
                    }
                    // normal keystroke; we ignore release repeats.
                    Some(Event::Key(k)) if k.kind != KeyEventKind::Release => k,
                    Some(other) => {
                        // key release, resize, ... -> plain redraw
                        if let Event::Resize(w, h) = other {
                            agent_tui::debug_log::log(&format!("event: resize {w}x{h}"));
                        }
                        continue;
                    }
                };
                #[cfg(feature = "codex_tui_parity")]
                let action = chat.route_key(&mut state, k);
                #[cfg(not(feature = "codex_tui_parity"))]
                let action = state.on_key(k);
                match action {
                    InputAction::Submit(prompt) => {
                        // US-017 AC2: the submission is gated by its hooks before
                        // anything else happens. A refusal keeps the message in the
                        // composer and names the reason, so the user can amend it.
                        if cfg.hooks.watches(agent_tools::HookEvent::UserPromptSubmit) {
                            let hooks = Arc::clone(&cfg.hooks);
                            if let agent_tools::HookDecision::Deny(reason) = hooks
                                .lifecycle(agent_tools::Lifecycle::UserPromptSubmit {
                                    prompt: &prompt,
                                })
                                .await
                            {
                                state.blocks.push(Block::Error(format!("Prompt refused: {reason}")));
                                state.set_input(prompt);
                                continue;
                            }
                        }
                        // US-016: `/<skill> …` injects the skill instructions instead
                        // of sending its name. Resolved HERE, at submission, so an
                        // unreadable skill blocks the turn while the user is looking.
                        // The body enters the STEP context, so it reaches the next
                        // model request whether this input opened a turn or steered
                        // one, and it is still never persisted.
                        let skill = match crate::skills::invocation(&cfg.skills, &prompt) {
                            Some(Ok(injection)) => {
                                if injection.truncated {
                                    state.blocks.push(Block::Notice(format!(
                                        "Skill \"{}\" injected, body truncated at the byte budget.",
                                        injection.name
                                    )));
                                }
                                Some((injection.name, injection.block))
                            }
                            Some(Err(err)) => {
                                state
                                    .blocks
                                    .push(Block::Error(format!("Skill unusable: {err}")));
                                // Nothing is sent, so the typed message goes back
                                // to the composer instead of being lost.
                                state.set_input(prompt);
                                continue;
                            }
                            None => None,
                        };
                        let injected = skill
                            .as_ref()
                            .map(|(name, body)| {
                                let section = format!("skill:{name}");
                                cfg.steps.inject(section.clone(), body.clone());
                                section
                            });
                        // AC6: `expected_turn_id = None` accepts EITHER branch of
                        // the steer/terminal race, which is what a typed message
                        // needs: it steers the turn that is running, or opens one
                        // of its own if that turn ended meanwhile. Never a
                        // post-turn FIFO, and never lost.
                        let was_running = running;
                        match runtime.steer(Submission::new(prompt.clone()), None).await {
                            Ok(_) => {
                                if !was_running {
                                    goal_iters = 0;
                                }
                                state.push_user(prompt.clone());
                                #[cfg(feature = "codex_tui_parity")]
                                chat.push_user_message(&state, prompt.clone());
                                if was_running {
                                    state.blocks.push(Block::Notice(
                                        "Steering the current turn.".into(),
                                    ));
                                }
                            }
                            Err(err) => {
                                // Only what this input added: another injection
                                // may belong to the turn that is running.
                                if let Some(section) = &injected {
                                    cfg.steps.remove_injection(section);
                                }
                                // AC8: a store that refuses is a named error and
                                // the session goes on; a runtime that STOPPED is
                                // terminal, because nothing can run any more.
                                if matches!(err, SubmitError::Stopped) {
                                    runtime_failure = Some(format!("runtime stopped: {err}"));
                                    state.should_quit = true;
                                }
                                state
                                    .blocks
                                    .push(Block::Error(format!("Input refused: {err}")));
                                state.set_input(prompt);
                            }
                        }
                        status = runtime.status();
                        running = is_running(&status);
                        apply_runtime_status(&mut state, &status);
                    }
                    InputAction::Command(line) => {
                        let mut it = line.splitn(2, ' ');
                        let cmd = it.next().unwrap_or("");
                        let arg = it.next().unwrap_or("").trim();
                        match cmd {
                            "/help" => {
                                let list = COMMANDS
                                    .iter()
                                    .map(|(n, _, _)| *n)
                                    .collect::<Vec<_>>()
                                    .join("  ");
                                state.blocks.push(Block::Notice(format!("Commands: {list}")));
                            }
                            "/models" => {
                                if arg.is_empty() {
                                    state.blocks.push(Block::Notice(
                                        "Usage : /models <slug> (ex: /models gpt-5.5)".into(),
                                    ));
                                } else {
                                    let removed = count_encrypted_reasoning(&runtime.messages());
                                    if removed > 0
                                        && let Err(e) =
                                            runtime.session().redact_encrypted_reasoning().await
                                    {
                                        state.blocks.push(Block::Error(format!(
                                            "models: redaction reasoning: {e}"
                                        )));
                                        continue;
                                    }
                                    if removed > 0 {
                                        let _ = runtime
                                            .conversation()
                                            .lock()
                                            .map(|mut msgs| scrub_encrypted_reasoning(&mut msgs[..]));
                                    }
                                    let previous_effort = cfg.reasoning_effort.clone();
                                    let next_effort = previous_effort
                                        .as_deref()
                                        .and_then(|effort| {
                                            normalize_reasoning_effort_for_model(arg, effort)
                                        })
                                        .or_else(|| {
                                            default_reasoning_effort_for_model(arg)
                                                .map(str::to_string)
                                        });
                                    cfg.model = arg.to_string();
                                    cfg.reasoning_effort = next_effort.clone();
                                    state.model = arg.to_string();
                                    state.reasoning_effort = next_effort.clone();
                                    let suffix = if removed > 0 {
                                        format!(" ({removed} reasoning items removed)")
                                    } else {
                                        String::new()
                                    };
                                    let effort_suffix = next_effort
                                        .as_deref()
                                        .map(|effort| {
                                            format!(" [{}]", reasoning_effort_label(effort))
                                        })
                                        .unwrap_or_default();
                                    state.blocks.push(Block::Notice(format!(
                                        "Model: {arg}{effort_suffix}{suffix}"
                                    )));
                                    persist_model(&mut state, cfg.settings_path.as_deref(), arg);
                                    persist_reasoning_effort(
                                        &mut state,
                                        cfg.settings_path.as_deref(),
                                        next_effort.as_deref(),
                                    );
                                    sync_settings(&cfg);
                                }
                            }
                            "/effort" => {
                                let supported = supported_reasoning_efforts_for_model(&cfg.model);
                                if arg.is_empty() {
                                    if supported.is_empty() {
                                        state.blocks.push(Block::Notice(format!(
                                            "No known reasoning efforts for model {}",
                                            cfg.model
                                        )));
                                    } else {
                                        state.blocks.push(Block::Notice(format!(
                                            "Usage : /effort <{}>",
                                            supported.join("|")
                                        )));
                                    }
                                } else if let Some(effort) =
                                    normalize_reasoning_effort_for_model(&cfg.model, arg)
                                {
                                    cfg.reasoning_effort = Some(effort.clone());
                                    state.reasoning_effort = Some(effort.clone());
                                    state.blocks.push(Block::Notice(format!(
                                        "Reasoning effort: {}",
                                        reasoning_effort_label(&effort)
                                    )));
                                    persist_reasoning_effort(
                                        &mut state,
                                        cfg.settings_path.as_deref(),
                                        Some(&effort),
                                    );
                                    sync_settings(&cfg);
                                } else if supported.is_empty() {
                                    state.blocks.push(Block::Notice(format!(
                                        "No known reasoning efforts for model {}",
                                        cfg.model
                                    )));
                                } else {
                                    state.blocks.push(Block::Notice(format!(
                                        "Unsupported reasoning effort for {}: {arg}. Available: {}",
                                        cfg.model,
                                        supported.join("|")
                                    )));
                                }
                            }
                            "/permissions" => {
                                if arg.is_empty() {
                                    state.blocks.push(Block::Notice(
                                        "Usage : /permissions <ask|accept-edits|auto|full-access|read-only>"
                                            .into(),
                                    ));
                                } else if let Some(mode) = permission_mode_from_arg(arg) {
                                    cfg.permission_mode.set(mode);
                                    let id = permission_mode_id(mode);
                                    state.set_permission_mode(id);
                                    let label = permission_mode_label(id);
                                    let message = format!("Permissions updated to {label}");
                                    state.blocks.push(Block::Notice(message.clone()));
                                    if let Some(path) = &cfg.settings_path
                                        && let Err(err) =
                                            crate::settings::save_permission_mode(path, mode)
                                    {
                                        state.blocks.push(Block::Error(format!(
                                            "settings: failed to save permission mode: {err}"
                                        )));
                                    }
                                    sync_settings(&cfg);
                                } else {
                                    state.blocks.push(Block::Notice(format!(
                                        "Unknown permission mode: {arg}"
                                    )));
                                }
                            }
                            "/goal" if running => state.blocks.push(Block::Notice(
                                "Wait for the current turn to finish.".into(),
                            )),
                            "/goal" => match arg {
                                "" => state.blocks.push(Block::Notice(match &cfg.goal {
                                    Some(g) => format!("Active goal: {g}"),
                                    None => "No goal. Usage: /goal <goal to complete>".into(),
                                })),
                                "clear" => {
                                    cfg.goal = None;
                                    sync_settings(&cfg);
                                    if let Err(e) = remove_if_exists(&goal_path) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = remove_if_exists(&goal_iters_path) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    state.blocks.push(Block::Notice("Goal cleared.".into()));
                                }
                                g => {
                                    // Sets the goal of this session and starts the work.
                                    cfg.goal = Some(g.to_string());
                                    sync_settings(&cfg);
                                    if let Err(e) = std::fs::write(&goal_path, g) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = write_goal_iters(&goal_iters_path, 0) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    goal_iters = 0;
                                    match runtime.submit(Submission::new(g)).await {
                                        Ok(_) => {
                                            state.push_user(g);
                                            #[cfg(feature = "codex_tui_parity")]
                                            chat.push_user_message(&state, g.to_string());
                                        }
                                        Err(err) => state
                                            .blocks
                                            .push(Block::Error(format!("goal refused: {err}"))),
                                    }
                                    status = runtime.status();
                                    running = is_running(&status);
                                    apply_runtime_status(&mut state, &status);
                                }
                            },
                            // A branch is cut at a TERMINAL turn boundary, so these
                            // wait for the current turn rather than copying a moving
                            // transcript (edge case #12).
                            "/resume" | "/new" | "/clear" | "/fork" | "/rewind" if running => {
                                state.blocks.push(Block::Notice(
                                    "Wait for the current turn to finish.".into(),
                                ));
                            }
                            // US-017 AC2/AC3/AC4: the branch is asked of the RUNTIME,
                            // at a named terminal boundary or at the last one. The
                            // source thread is neither truncated, rewritten nor
                            // deleted: it stays on disk exactly as it was, and the
                            // client switches to the branch.
                            "/fork" | "/rewind" => {
                                let at = match parse_turn_argument(arg) {
                                    Ok(at) => at,
                                    Err(err) => {
                                        state.blocks.push(Block::Error(err));
                                        continue;
                                    }
                                };
                                if cmd == "/rewind" && at.is_none() {
                                    state.blocks.push(Block::Notice(
                                        "Usage: /rewind <turn-id> (see /status for the current turn)"
                                            .into(),
                                    ));
                                    continue;
                                }
                                let branch = match runtime.fork(at).await {
                                    Ok(branch) => branch,
                                    Err(err) => {
                                        state
                                            .blocks
                                            .push(Block::Error(format!("{cmd}: {err}")));
                                        continue;
                                    }
                                };
                                let Some(path) = branch.path.clone() else {
                                    state.blocks.push(Block::Error(format!(
                                        "{cmd}: this session persists nothing to branch from"
                                    )));
                                    continue;
                                };
                                runtime.shutdown().await;
                                match open_session(&cfg, &path, root).await {
                                    Ok(opened) => {
                                        runtime = opened.runtime;
                                        events = opened.events;
                                        status = opened.status;
                                        running = is_running(&status);
                                        current_session = path;
                                        goal_path = goal_path_for_session(&current_session);
                                        goal_iters_path =
                                            goal_iters_path_for_session(&current_session);
                                        // The branch starts from the current state,
                                        // goal included: a branch that dropped it
                                        // would not be the same session.
                                        if let Some(goal) = cfg.goal.clone() {
                                            if let Err(e) = std::fs::write(&goal_path, &goal) {
                                                state.blocks.push(Block::Error(format!(
                                                    "{cmd}: goal: {e}"
                                                )));
                                            }
                                            if let Err(e) =
                                                write_goal_iters(&goal_iters_path, goal_iters)
                                            {
                                                state.blocks.push(Block::Error(format!(
                                                    "{cmd}: goal: {e}"
                                                )));
                                            }
                                        }
                                        let msgs = runtime.messages();
                                        state.blocks = blocks_from_messages(&msgs);
                                        #[cfg(feature = "codex_tui_parity")]
                                        {
                                            chat.replace_messages(&msgs);
                                        }
                                        state.blocks.push(Block::Notice(format!(
                                            "Branch {} created at turn {} ({} messages). The \
                                             source thread stays on disk untouched.",
                                            branch.thread_id,
                                            branch.fork_turn_id,
                                            msgs.len()
                                        )));
                                        state.sessions =
                                            load_sessions(&sessions_dir, &current_session);
                                        apply_runtime_status(&mut state, &status);
                                    }
                                    Err(err) => {
                                        runtime_failure =
                                            Some(format!("{cmd}: branch unusable: {err}"));
                                        state.should_quit = true;
                                    }
                                }
                            }
                            "/resume" | "/new" | "/clear" => {
                                let path = if cmd == "/resume" {
                                    match crate::resolve_resume_path(&sessions_dir, arg) {
                                        Ok(path) => path,
                                        Err(e) => {
                                            state.blocks.push(Block::Error(format!("{e}")));
                                            continue;
                                        }
                                    }
                                } else {
                                    new_session_path(&sessions_dir)
                                };
                                runtime.shutdown().await;
                                match open_session(&cfg, &path, root).await {
                                    Ok(opened) => {
                                        runtime = opened.runtime;
                                        events = opened.events;
                                        status = opened.status;
                                        running = is_running(&status);
                                        current_session = path;
                                        goal_path = goal_path_for_session(&current_session);
                                        goal_iters_path =
                                            goal_iters_path_for_session(&current_session);
                                        cfg.goal = if cmd == "/resume" {
                                            read_goal(&goal_path)
                                        } else {
                                            None
                                        };
                                        sync_settings(&cfg);
                                        goal_iters = if cfg.goal.is_some() {
                                            read_goal_iters(&goal_iters_path)
                                        } else {
                                            0
                                        };
                                        if cmd != "/resume" {
                                            if let Err(e) = remove_if_exists(&goal_path) {
                                                state
                                                    .blocks
                                                    .push(Block::Error(format!("goal: {e}")));
                                            }
                                            if let Err(e) = remove_if_exists(&goal_iters_path) {
                                                state
                                                    .blocks
                                                    .push(Block::Error(format!("goal: {e}")));
                                            }
                                        }
                                        let msgs = runtime.messages();
                                        cfg.engine.tools.seed_taint(recent_untrusted_content(
                                            &msgs,
                                            crate::RESUME_TAINT_SCAN_MESSAGES,
                                        ));
                                        state.blocks = blocks_from_messages(&msgs);
                                        #[cfg(feature = "codex_tui_parity")]
                                        {
                                            chat.replace_messages(&msgs);
                                        }
                                        // A cleared transcript brings the welcome
                                        // screen back, which is its own confirmation.
                                        if !msgs.is_empty() {
                                            state.blocks.push(Block::Notice(format!(
                                                "Session resumed ({} messages).",
                                                msgs.len()
                                            )));
                                        } else if cmd == "/resume" {
                                            state
                                                .blocks
                                                .push(Block::Notice("Empty session.".into()));
                                        }
                                        state.sessions =
                                            load_sessions(&sessions_dir, &current_session);
                                        apply_runtime_status(&mut state, &status);
                                    }
                                    Err(err) => {
                                        runtime_failure =
                                            Some(format!("{cmd}: session unusable: {err}"));
                                        state.should_quit = true;
                                    }
                                }
                            }
                            // US-005: purely local surfaces (no network call), pushed
                            // as notices so they never enter the transcript.
                            "/status" => {
                                let session_id = current_session
                                    .file_name()
                                    .map(|name| name.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let sources = config_sources_still_valid(
                                    &cfg,
                                    &resolved_model,
                                    &resolved_effort,
                                    resolved_permission_mode,
                                );
                                state.blocks.push(Block::Notice(
                                    agent_tui::session_status_report(
                                        &state,
                                        agent_tui::SessionFacts {
                                            session_id: &session_id,
                                            sandbox: &cfg.sandbox_scope,
                                            config_sources: &sources,
                                            profile: cfg.profile.as_deref(),
                                            runtime: runtime_facts(),
                                        },
                                    ),
                                ));
                            }
                            "/usage" => {
                                let report = agent_tui::session_usage_report(&state);
                                state.blocks.push(Block::Notice(report));
                            }
                            // US-009: inspection surface of the session memory.
                            // Local only, and the memory never leaves the process.
                            "/approvals" => {
                                let report = match arg {
                                    "clear" => {
                                        let n = cfg.approvals.clear();
                                        format!("{n} remembered answer(s) forgotten.")
                                    }
                                    "" => approvals_report(&cfg.approvals.entries()),
                                    other => format!(
                                        "Unknown argument: {other}. Usage: /approvals [clear]"
                                    ),
                                };
                                state.blocks.push(Block::Notice(report));
                            }
                            // US-006: the diff reuses the engine of the aggregated turn
                            // diff, hence exactly its scope.
                            "/diff" => {
                                match agent_tools::turn_diff::workspace_diff(&cfg.workspace).await {
                                    Ok(agent_tools::turn_diff::WorkspaceDiff::NoRepository) => {
                                        state.blocks.push(Block::Notice(
                                            "Diff unavailable: this directory is not a git \
                                             repository."
                                                .into(),
                                        ));
                                    }
                                    Ok(agent_tools::turn_diff::WorkspaceDiff::Changes(diff)) => {
                                        state.blocks.push(Block::Notice(workspace_diff_report(
                                            &diff,
                                        )));
                                    }
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("diff: {e}")));
                                    }
                                }
                            }
                            // US-019: the last answer as it was streamed, no
                            // rendering applied. A clipboard that refuses is an
                            // error block, never a silent "copied".
                            "/copy" => match last_assistant_text(&state) {
                                None => state
                                    .blocks
                                    .push(Block::Notice("No answer to copy yet.".into())),
                                Some(text) if text.is_empty() => state
                                    .blocks
                                    .push(Block::Notice("The last answer is empty.".into())),
                                Some(text) => match copy_to_clipboard(&text).await {
                                    Ok(helper) => state.blocks.push(Block::Notice(format!(
                                        "Last answer copied with {helper} ({} characters).",
                                        text.chars().count()
                                    ))),
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("copy: {e}")))
                                    }
                                },
                            },
                            // US-019: hooks are declared in the GLOBAL settings only,
                            // so this list is also the answer to "can this repository
                            // run something behind my back?".
                            "/hooks" => state
                                .blocks
                                .push(Block::Notice(hooks_report(&cfg.hook_specs))),
                            "/logout" if !state.provider_connected => state
                                .blocks
                                .push(Block::Notice("Already signed out.".into())),
                            "/logout" => match sign_out(&cfg.provider).await {
                                Ok(()) => {
                                    state.provider_connected = false;
                                    state.blocks.push(Block::Notice(format!(
                                        "Signed out: local credential deleted. \
                                         {LOGOUT_SERVER_NOTE}"
                                    )));
                                }
                                Err(e) => {
                                    state.blocks.push(Block::Error(format!("logout: {e}")))
                                }
                            },
                            "/init" if running => state.blocks.push(Block::Notice(
                                "Wait for the current turn to finish.".into(),
                            )),
                            "/init" => match init_decision(&cfg.workspace, arg) {
                                InitDecision::Confirm(name) => {
                                    state.blocks.push(Block::Notice(format!(
                                        "{name} already exists at the workspace root. Run \
                                         `/init force` to have it rewritten."
                                    )));
                                }
                                decision => {
                                    if let InitDecision::Overwrite(name) = decision {
                                        state.blocks.push(Block::Notice(format!(
                                            "Rewriting {name} (confirmed by `force`)."
                                        )));
                                    }
                                    // The transcript keeps `/init`; the instructions
                                    // travel as a step section, exactly like a skill
                                    // body, so they are never persisted.
                                    cfg.steps.inject("init", INIT_PROMPT.to_string());
                                    match runtime.submit(Submission::new(cmd)).await {
                                        Ok(_) => {
                                            state.push_user(cmd);
                                            #[cfg(feature = "codex_tui_parity")]
                                            chat.push_user_message(&state, cmd.to_string());
                                            // AC1: the project context was read before
                                            // this turn wrote the file, so it is
                                            // re-read when the turn ends, no restart
                                            // involved.
                                            refresh_context = true;
                                        }
                                        Err(err) => {
                                            cfg.steps.clear_injections();
                                            state
                                                .blocks
                                                .push(Block::Error(format!("init refused: {err}")));
                                        }
                                    }
                                    status = runtime.status();
                                    running = is_running(&status);
                                    apply_runtime_status(&mut state, &status);
                                }
                            },
                            "/compact" if running => state.blocks.push(Block::Notice(
                                "A turn is in progress.".into(),
                            )),
                            "/compact" => {
                                let mut msgs = runtime.messages();
                                let before = msgs.len();
                                let max_output_tokens =
                                    cfg.settings.read(|s| s.run_config.max_output_tokens);
                                match agent_core::compaction::full_compact(
                                    &mut msgs,
                                    &cfg.model,
                                    cfg.provider.as_ref(),
                                    max_output_tokens,
                                )
                                .await
                                {
                                    // Persisted like an automatic compaction: same
                                    // checkpoint entry, hence replayable by `/resume`
                                    // without the session knowing it was manual.
                                    Ok(_) => match runtime
                                        .session()
                                        .checkpoint(agent_core::CompactKind::Auto, &msgs)
                                        .await
                                    {
                                        Ok(()) => {
                                            let after = msgs.len();
                                            if let Ok(mut g) = runtime.conversation().lock() {
                                                *g = msgs;
                                            }
                                            state.blocks.push(Block::Notice(format!(
                                                "Context compacted ({before} → {after} messages)."
                                            )));
                                        }
                                        Err(e) => state
                                            .blocks
                                            .push(Block::Error(format!("compact: {e}"))),
                                    },
                                    // `full_compact` leaves the transcript intact on
                                    // failure: the session stays usable as is.
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("compact: {e}")));
                                    }
                                }
                            }
                            "/providers" => match arg {
                                "apikey" => state.blocks.push(Block::Notice(
                                    "API key authentication is coming soon.".into(),
                                )),
                                "subscription anthropic" => state.blocks.push(Block::Notice(
                                    "Anthropic (Claude Pro/Max) is coming soon.".into(),
                                )),
                                "subscription codex connect" => {
                                    if state.provider_connected {
                                        state
                                            .blocks
                                            .push(Block::Notice("Already connected to Codex.".into()));
                                    } else {
                                        state.blocks.push(Block::Notice(
                                            "Quit and restart `pyxis`: the built-in onboarding \
                                             will reconnect ChatGPT."
                                                .into(),
                                        ));
                                    }
                                }
                                // US-019: same sign-out path as `/logout`, so the two
                                // surfaces cannot promise different things.
                                "subscription codex disconnect" => {
                                    if state.provider_connected {
                                        match sign_out(&cfg.provider).await {
                                            Ok(()) => {
                                                state.provider_connected = false;
                                                state.blocks.push(Block::Notice(format!(
                                                    "Disconnected from Codex (credential \
                                                     removed). Log in again before the next \
                                                     model call. {LOGOUT_SERVER_NOTE}"
                                                )));
                                            }
                                            Err(e) => state
                                                .blocks
                                                .push(Block::Error(format!("disconnect: {e}"))),
                                        }
                                    } else {
                                        state
                                            .blocks
                                            .push(Block::Notice("Already disconnected.".into()));
                                    }
                                }
                                "" | "subscription" | "subscription codex" => {
                                    state.blocks.push(Block::Notice(
                                        "Choose a provider and then an action in the submenu."
                                            .into(),
                                    ))
                                }
                                other => state
                                    .blocks
                                    .push(Block::Notice(format!("Unknown provider: {other}"))),
                            },
                            "/mcp" => {
                                handle_mcp(
                                    arg,
                                    &mcp,
                                    &mcp_tx,
                                    &cfg.command_hardener,
                                    &registry,
                                    &mut mcp_tool_names,
                                    &mut state,
                                )
                            }
                            "/skills" => state.blocks.push(Block::Notice(
                                "Choose a skill in the /skills submenu.".into(),
                            )),
                            "/quit" => show_shutdown_feedback(
                                &mut state,
                                &mut pending_resp,
                                &mut turn_start,
                            ),
                            other => state
                                .blocks
                                .push(Block::Notice(format!("Unknown command: {other}"))),
                        }
                        state.scroll = 0;
                    }
                    InputAction::Quit => show_shutdown_feedback(
                        &mut state,
                        &mut pending_resp,
                        &mut turn_start,
                    ),
                    InputAction::Interrupt if running => {
                        if let Some(resp) = pending_resp.take() {
                            let _ = resp.send(agent_tools::permission::ApprovalResponse::DENY_ONCE);
                        }
                        // The runtime signals the turn's own cancellation node and
                        // acknowledges at once; the terminal is written after the
                        // model, the tools and their process trees have stopped and
                        // the transcript was reconciled.
                        if let Err(err) = runtime.interrupt(None).await {
                            state
                                .blocks
                                .push(Block::Error(format!("interrupt: {err}")));
                        }
                    }
                    InputAction::Interrupt => {}
                    InputAction::Permission { allow, remember } => {
                        if let Some(resp) = pending_resp.take() {
                            let _ = resp.send(agent_tools::permission::ApprovalResponse {
                                allow,
                                remember,
                            });
                        }
                        #[cfg(feature = "codex_tui_parity")]
                        chat.record_approval_decision(allow);
                    }
                    _ => {}
                }
            }
            received = events.recv() => {
                match received {
                    Ok(event) => {
                        match event.payload {
                            // US-005 AC2: the engine event is forwarded with its
                            // canonical content untouched; only its correlation is
                            // added, and the frontend renders what it always did.
                            RuntimeEventPayload::Engine(ev) => {
                                // Calibration probe (US-002): in interactive mode the
                                // TUI owns the terminal, so the line goes to the debug
                                // log, never to a process output.
                                if let AgentEvent::ModelTurn(view) = &ev
                                    && let Some(line) = crate::jsonl::usage_probe_line(view)
                                {
                                    agent_tui::debug_log::log(&line);
                                }
                                #[cfg(feature = "codex_tui_parity")]
                                chat.sync_local_blocks(&state);
                                state.apply(&ev);
                                #[cfg(feature = "codex_tui_parity")]
                                chat.handle_agent_event(&state, &ev);
                            }
                            RuntimeEventPayload::TurnStateChanged { to, ref cause, .. } => {
                                if to == TurnState::Running {
                                    // Diff reference taken when the turn really
                                    // starts, hence before its first tool write.
                                    diff_tracker = Some(
                                        agent_tools::turn_diff::TurnDiffTracker::begin(
                                            &cfg.workspace,
                                        )
                                        .await,
                                    );
                                }
                                if to.is_terminal() {
                                    // Aggregated after the last tool write, including
                                    // when the turn was interrupted.
                                    if let Some(mut tracker) = diff_tracker.take() {
                                        match tracker.turn_diff().await {
                                            Ok(diff) if !diff.is_empty() => {
                                                let ev = AgentEvent::TurnDiff(diff);
                                                #[cfg(feature = "codex_tui_parity")]
                                                chat.sync_local_blocks(&state);
                                                state.apply(&ev);
                                                #[cfg(feature = "codex_tui_parity")]
                                                chat.handle_agent_event(&state, &ev);
                                            }
                                            Ok(_) => {}
                                            Err(err) => agent_tui::debug_log::log(&format!(
                                                "turn diff: {err}"
                                            )),
                                        }
                                    }
                                    // US-019 AC1: a terminal cause reaches the
                                    // transcript with the SAME category, the
                                    // same next step and the same identifiers
                                    // the other three surfaces show. Dropping it
                                    // here is what used to make a failed turn
                                    // look like no reaction at all.
                                    if let Some(failure) =
                                        agent_runtime::TurnFailure::classify(to, cause.as_deref())
                                    {
                                        let line = crate::failure_line::render(
                                            &failure,
                                            event.thread_id,
                                            event.turn_id,
                                        );
                                        // A cancellation or a shutdown is not a
                                        // fault: it says so in words either way,
                                        // but tinting it red would report a
                                        // failure the user caused on purpose.
                                        state.blocks.push(
                                            match failure.category {
                                                agent_runtime::FailureCategory::Interrupted => {
                                                    Block::Notice(line)
                                                }
                                                _ => Block::Error(line),
                                            },
                                        );
                                    }
                                    // US-019 AC1 of the harness PRD: re-read BEFORE any
                                    // continuation turn, so the goal loop sees the
                                    // fresh context too.
                                    if std::mem::take(&mut refresh_context) {
                                        cfg.steps.set_project(crate::context::project_messages(
                                            &cfg.workspace,
                                            &crate::context::today_utc(),
                                            &cfg.skills,
                                        ));
                                        state.blocks.push(Block::Notice(
                                            match crate::context::instructions_file(&cfg.workspace) {
                                                Some(name) => format!(
                                                    "{name} is part of the project context from \
                                                     the next model request on."
                                                ),
                                                None => "No instruction file written: the project \
                                                         context is unchanged."
                                                    .to_string(),
                                            },
                                        ));
                                    }
                                    // The runtime may already have started the next
                                    // queued input: what it says is what counts.
                                    status = runtime.status();
                                    running = is_running(&status);
                                    // A body injected for a turn survives an input
                                    // that opens the next one immediately, and is
                                    // dropped when the thread goes quiet.
                                    if !running {
                                        cfg.steps.clear_injections();
                                    }
                                    // Goal loop: on a CLEAN end of turn with an active
                                    // goal and nothing else queued, we re-prompt as
                                    // long as the completion marker is not emitted.
                                    if to == TurnState::Completed && cfg.goal.is_some() && !running {
                                        if take_goal_done(&mut state) {
                                            cfg.goal = None;
                                            sync_settings(&cfg);
                                            if let Err(e) = remove_if_exists(&goal_path) {
                                                state
                                                    .blocks
                                                    .push(Block::Error(format!("goal: {e}")));
                                            }
                                            if let Err(e) = remove_if_exists(&goal_iters_path) {
                                                state
                                                    .blocks
                                                    .push(Block::Error(format!("goal: {e}")));
                                            }
                                            state.blocks.push(Block::Notice(
                                                "Goal completed and cleared.".into(),
                                            ));
                                        } else if goal_iters < MAX_GOAL_ITERS {
                                            goal_iters += 1;
                                            if let Err(e) =
                                                write_goal_iters(&goal_iters_path, goal_iters)
                                            {
                                                state
                                                    .blocks
                                                    .push(Block::Error(format!("goal: {e}")));
                                            } else {
                                                state.blocks.push(Block::Notice(format!(
                                                    "Continuing goal \
                                                     ({goal_iters}/{MAX_GOAL_ITERS})..."
                                                )));
                                                turn_start = None;
                                                state.end_turn();
                                                // A continuation is an INPUT like any
                                                // other: durable before it is
                                                // acknowledged (FR-05), so the log
                                                // says why the next turn exists.
                                                if let Err(err) = runtime
                                                    .submit(Submission::new(GOAL_CONTINUE_PROMPT))
                                                    .await
                                                {
                                                    state.blocks.push(Block::Error(format!(
                                                        "goal: continuation refused: {err}"
                                                    )));
                                                }
                                            }
                                        } else {
                                            state.blocks.push(Block::Notice(format!(
                                                "Goal not confirmed after {MAX_GOAL_ITERS} \
                                                 retries. Use /goal clear to abandon it."
                                            )));
                                        }
                                        status = runtime.status();
                                        running = is_running(&status);
                                    }
                                    // `Stop` fires when the agent really stops, hence
                                    // not when the goal loop or a queued input opens
                                    // another turn right away.
                                    if !running && cfg.hooks.watches(agent_tools::HookEvent::Stop) {
                                        let hooks = Arc::clone(&cfg.hooks);
                                        hooks.lifecycle(agent_tools::Lifecycle::Stop).await;
                                    }
                                }
                            }
                            // The input is already in the transcript: it was displayed
                            // when it was accepted.
                            RuntimeEventPayload::InputAccepted { .. }
                            | RuntimeEventPayload::Forked { .. }
                            | RuntimeEventPayload::ShuttingDown => {}
                            RuntimeEventPayload::StoreFailed { operation, detail } => {
                                let failure =
                                    format!("thread store failed during {operation}: {detail}");
                                state.blocks.push(Block::Error(failure.clone()));
                                runtime_failure = Some(failure);
                                state.should_quit = true;
                            }
                        }
                        status = runtime.status();
                        running = is_running(&status);
                        apply_runtime_status(&mut state, &status);
                        // AC8: the actor closed its admission without being asked
                        // to. Nothing can run any more, so the session ends on a
                        // named error rather than on a composer that accepts
                        // input nobody will read.
                        if status.shutting_down && !state.should_quit {
                            runtime_failure = Some(
                                "the thread runtime closed its admission: no further turn can run"
                                    .into(),
                            );
                            state.should_quit = true;
                        }
                    }
                    // The durable state is the store, so a dropped LIVE event costs
                    // the display a line, not the session (edge case #18).
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        state.blocks.push(Block::Notice(format!(
                            "{dropped} runtime event(s) dropped from the live stream; the \
                             transcript on disk stays complete."
                        )));
                        status = runtime.status();
                        if let agent_runtime::ThreadHealth::StoreFailed { operation, detail } =
                            &status.health
                        {
                            let failure =
                                format!("thread store failed during {operation}: {detail}");
                            state.blocks.push(Block::Error(failure.clone()));
                            runtime_failure = Some(failure);
                            state.should_quit = true;
                        }
                        running = is_running(&status);
                        apply_runtime_status(&mut state, &status);
                    }
                    // AC8: the runtime went away. The terminal is restored by `run`,
                    // and the session ends on a named error instead of a panic or a
                    // silent freeze.
                    Err(broadcast::error::RecvError::Closed) => {
                        runtime_failure =
                            Some("the thread runtime stopped: no further turn can run".into());
                        state.should_quit = true;
                    }
                }
            }
            perm = perm_rx.recv() => {
                if let Some((req, resp)) = perm {
                    state.pending = Some(to_prompt(&req));
                    #[cfg(feature = "codex_tui_parity")]
                    {
                        chat.handle_permission_request(PermissionTranscriptRequest {
                            call_id: req.call_id.clone(),
                            tool: req.tool.clone(),
                            reason: req.reason.clone(),
                            taint_forced: req.taint_forced,
                            input_summary: req.input_summary.clone(),
                            mode: req.mode.clone(),
                            input: req.input.clone(),
                        });
                    }
                    pending_resp = Some(resp);
                }
            }
            ev = mcp_rx.recv() => {
                if let Some(ev) = ev {
                    match ev {
                        McpEvent::Connected { name, conn, tools } => {
                            // US-014: the policy of THIS server shapes what is exposed,
                            // before anything reaches the tool registry.
                            let policy = mcp_config_for(&mcp, &name)
                                .map(|cfg| cfg.tools)
                                .unwrap_or_default();
                            let (tools, filter_notices) =
                                agent_mcp::filter_tools(&name, &tools, &policy);
                            for notice in filter_notices {
                                state.blocks.push(Block::Notice(notice));
                            }
                            // Reconnect: the names this server held are released (and
                            // staged out) before new ones are handed out, so a name is
                            // never handed to two tools.
                            if let Some(previous) = mcp_tool_names.remove(&name) {
                                registry.stage_removal(previous.into_iter().collect());
                            }
                            let mut taken: BTreeSet<String> =
                                mcp_tool_names.values().flatten().cloned().collect();
                            let client = conn.client(&name);
                            let (exposed, skipped) =
                                agent_mcp::dyn_tools(&name, &tools, &policy, &client, &mut taken);
                            for skip in skipped {
                                state.blocks.push(Block::Notice(skip.summary()));
                            }
                            let n = exposed.len();
                            let names: BTreeSet<String> =
                                exposed.iter().map(|tool| tool.name().to_string()).collect();
                            // Poisoned lock: close the connection instead of silently dropping it.
                            match mcp.lock() {
                                Ok(mut r) => {
                                    if let Some(c) = r.finish_connect(&name, conn, tools) {
                                        // Disconnected while connecting: cancel the orphan session.
                                        tokio::spawn(async move { c.cancel().await });
                                        state.blocks.push(Block::Notice(format!(
                                            "MCP \"{name}\": connection canceled."
                                        )));
                                    } else {
                                        // US-016: staged, not registered. The exposed set
                                        // only moves at a turn boundary, so a turn in
                                        // flight keeps the tools it was given.
                                        registry.stage_tools(exposed);
                                        mcp_tool_names.insert(name.clone(), names);
                                        state.blocks.push(Block::Notice(format!(
                                            "MCP \"{name}\" connected ({n} tools), callable from the next turn."
                                        )));
                                    }
                                }
                                Err(_) => {
                                    tokio::spawn(async move { conn.cancel().await });
                                    state.blocks.push(Block::Error(
                                        "MCP: registry unavailable, connection closed.".into(),
                                    ));
                                }
                            }
                        }
                        McpEvent::Failed { name, error } => {
                            if let Ok(mut r) = mcp.lock() {
                                r.fail(&name, error.clone());
                            }
                            state
                                .blocks
                                .push(Block::Error(format!("MCP \"{name}\": {error}")));
                        }
                    }
                    state.mcp_servers = mcp_metas(&mcp);
                }
            }
            notice = hook_notices.recv(), if hook_notices_open => {
                match notice {
                    Some(message) => state.blocks.push(Block::Notice(format!("Hook: {message}"))),
                    None => hook_notices_open = false,
                }
            }
            // Animation tick: wakes the loop ONLY during an ACTIVE turn
            // (`if running`) and outside a permission wait (`pending.is_none()`: we
            // are waiting for the human, not the agent -> 0 CPU, no 10 fps redraw of the dialog).
            // The redraw happens at the head of the loop; here we only advance the
            // animation state (spinner, duration). US-044.
            _ = spinner.tick(), if running && state.pending.is_none() => {
                let elapsed = turn_start.map(|t| t.elapsed()).unwrap_or_default();
                state.tick_progress(elapsed);
            }
            _ = tokio::time::sleep(state.quit_shortcut_remaining().unwrap_or(Duration::ZERO)), if state.quit_shortcut_hint_visible() => {
                state.clear_quit_shortcut_hint();
            }
        }
    }
    // Closes admission, cancels the tree, drains the tasks, writes the terminal
    // the running turn owes and closes the store. No task of this session
    // outlives the loop.
    runtime.shutdown().await;
    match runtime_failure {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
}

/// Handles `/mcp [<server> <action>]`. Connections (spawn + handshake) are
/// started in the background; the result comes back through `mcp_tx` into the loop.
#[allow(clippy::too_many_arguments)]
fn handle_mcp(
    arg: &str,
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    mcp_tx: &mpsc::Sender<McpEvent>,
    command_hardener: &agent_tools::CommandHardener,
    registry: &agent_tools::Registry,
    mcp_tool_names: &mut BTreeMap<String, BTreeSet<String>>,
    state: &mut AppState,
) {
    if arg == "issues" {
        show_mcp_issues(mcp, state);
        return;
    }
    let Some((server, action)) = arg.rsplit_once(' ') else {
        state.blocks.push(Block::Notice(
            "Select a server and then an action in the /mcp submenu. Diagnostics: /mcp issues."
                .into(),
        ));
        return;
    };
    let server = server.trim();
    if server.is_empty() {
        state
            .blocks
            .push(Block::Notice("Usage: /mcp <server> <action>.".into()));
        return;
    }
    match action {
        "connect" | "reconnect" => {
            if let Some(cfg) = mcp_config_for(mcp, server)
                && mcp_requires_trust(&cfg)
            {
                let lead =
                    format!("Connection blocked before spawn. Retry with /mcp {server} trust.");
                state
                    .blocks
                    .push(Block::Notice(mcp_trust_notice(server, &cfg, &lead)));
                return;
            }
            start_mcp_connect(server, mcp, mcp_tx, command_hardener, state, None);
        }
        "trust" => {
            let cfg = match mcp_config_for(mcp, server) {
                Some(cfg) => cfg,
                None => {
                    state
                        .blocks
                        .push(Block::Notice(format!("Unknown MCP server: {server}.")));
                    return;
                }
            };
            state.blocks.push(Block::Notice(mcp_trust_notice(
                server,
                &cfg,
                "Trust confirmed for this connection.",
            )));
            start_mcp_connect(server, mcp, mcp_tx, command_hardener, state, Some(cfg));
        }
        "disconnect" => {
            let old = mcp.lock().ok().and_then(|mut r| r.begin_disconnect(server));
            match old {
                Some(old) => {
                    tokio::spawn(async move { old.cancel().await });
                    // US-016 AC2: its tools leave the registry at the next turn
                    // boundary; a later call then fails as an unknown tool rather
                    // than reaching a dead connection.
                    let removed = mcp_tool_names.remove(server).unwrap_or_default();
                    let count = removed.len();
                    registry.stage_removal(removed.into_iter().collect());
                    state.blocks.push(Block::Notice(format!(
                        "MCP \"{server}\" disconnected ({count} tools withdrawn from the next turn on)."
                    )));
                }
                None => state
                    .blocks
                    .push(Block::Notice(format!("MCP \"{server}\" is not connected."))),
            }
            state.mcp_servers = mcp_metas(mcp);
        }
        "tools" => {
            if let Ok(reg) = mcp.lock() {
                match reg.get(server) {
                    Some(s) if !s.tools().is_empty() => {
                        let names = s
                            .tools()
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        state.blocks.push(Block::Notice(format!(
                            "MCP \"{server}\" ({} tools): {names}",
                            s.tools().len()
                        )));
                    }
                    Some(_) => state.blocks.push(Block::Notice(format!(
                        "MCP \"{server}\": no exposed tools."
                    ))),
                    None => state
                        .blocks
                        .push(Block::Notice(format!("Unknown MCP server: {server}."))),
                }
            }
        }
        other => state
            .blocks
            .push(Block::Notice(format!("Unknown MCP action: {other}"))),
    }
}

fn start_mcp_connect(
    server: &str,
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    mcp_tx: &mpsc::Sender<McpEvent>,
    command_hardener: &agent_tools::CommandHardener,
    state: &mut AppState,
    trusted_cfg: Option<agent_mcp::McpServerConfig>,
) {
    let begin = match mcp.lock() {
        Ok(mut r) => r.begin_connect(server),
        Err(_) => Err(agent_mcp::McpError::Unknown(server.to_string())),
    };
    match begin {
        Ok((cfg_srv, old)) => {
            // The whole config is compared, not just the command: a transport, an
            // endpoint or a tool policy swapped between the prompt and the spawn
            // would make the confirmation meaningless.
            if let Some(expected) = trusted_cfg
                && expected != cfg_srv
            {
                if let Some(old) = old {
                    tokio::spawn(async move { old.cancel().await });
                }
                if let Ok(mut r) = mcp.lock() {
                    r.fail(server, "MCP config changed during trust".to_string());
                }
                state.blocks.push(Block::Error(format!(
                    "MCP \"{server}\": config changed during trust."
                )));
                state.mcp_servers = mcp_metas(mcp);
                return;
            }
            if let Some(old) = old {
                tokio::spawn(async move { old.cancel().await });
            }
            state.mcp_servers = mcp_metas(mcp);
            state
                .blocks
                .push(Block::Notice(format!("MCP \"{server}\": connecting...")));
            let tx = mcp_tx.clone();
            let name = server.to_string();
            let harden = Arc::clone(command_hardener);
            tokio::spawn(async move {
                let ev = match agent_mcp::McpConnection::connect_hardened(
                    &name,
                    &cfg_srv,
                    Some(&harden),
                )
                .await
                {
                    Ok(conn) => match conn.list_tools(&name).await {
                        Ok(tools) => McpEvent::Connected { name, conn, tools },
                        Err(e) => {
                            conn.cancel().await;
                            McpEvent::Failed {
                                name,
                                error: e.to_string(),
                            }
                        }
                    },
                    Err(e) => McpEvent::Failed {
                        name,
                        error: e.to_string(),
                    },
                };
                // Closed channel: recover the connection and close the subprocess.
                if let Err(mpsc::error::SendError(ev)) = tx.send(ev).await
                    && let McpEvent::Connected { conn, .. } = ev
                {
                    conn.cancel().await;
                }
            });
        }
        Err(e) => state.blocks.push(Block::Notice(format!("MCP: {e}"))),
    }
}

fn mcp_config_for(
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    server: &str,
) -> Option<agent_mcp::McpServerConfig> {
    mcp.lock()
        .ok()
        .and_then(|r| r.get(server).map(|s| s.config().clone()))
}

fn show_mcp_issues(mcp: &Arc<Mutex<agent_mcp::McpRegistry>>, state: &mut AppState) {
    let Ok(reg) = mcp.lock() else {
        state
            .blocks
            .push(Block::Error("MCP: registry unavailable.".into()));
        return;
    };
    if reg.issues().is_empty() {
        state
            .blocks
            .push(Block::Notice("MCP: no config diagnostics.".into()));
        return;
    }
    let mut lines = reg
        .issues()
        .iter()
        .take(12)
        .map(agent_mcp::McpConfigIssue::summary)
        .collect::<Vec<_>>();
    if reg.issue_count() > lines.len() {
        lines.push(format!(
            "{} more diagnostics",
            reg.issue_count() - lines.len()
        ));
    }
    state.blocks.push(Block::Notice(format!(
        "Diagnostics MCP:\n{}",
        lines.join("\n")
    )));
}

/// Does this server need an explicit `/mcp <server> trust` before being spawned?
/// A workspace-controlled declaration, a config shadowing a user entry, and a
/// sensitive env key are the three cases where a repository could otherwise obtain
/// an execution. The startup connection (`main`) honors the same gate.
pub(crate) fn mcp_requires_trust(cfg: &agent_mcp::McpServerConfig) -> bool {
    matches!(cfg.source.origin, agent_mcp::McpConfigOrigin::Workspace)
        || cfg.shadows_lower_priority
        || !mcp_sensitive_env_keys(cfg).is_empty()
}

fn mcp_sensitive_env_keys(cfg: &agent_mcp::McpServerConfig) -> Vec<&str> {
    cfg.env()
        .keys()
        .map(String::as_str)
        .filter(|key| {
            let upper = key.to_ascii_uppercase();
            matches!(
                upper.as_str(),
                "PATH"
                    | "LD_PRELOAD"
                    | "LD_LIBRARY_PATH"
                    | "DYLD_INSERT_LIBRARIES"
                    | "DYLD_LIBRARY_PATH"
                    | "NODE_OPTIONS"
                    | "PYTHONPATH"
                    | "RUBYOPT"
                    | "BUNDLE_GEMFILE"
                    | "CARGO_HOME"
                    | "RUSTUP_HOME"
            )
        })
        .collect()
}

fn mcp_trust_notice(server: &str, cfg: &agent_mcp::McpServerConfig, lead: &str) -> String {
    let detail = match &cfg.transport {
        agent_mcp::McpTransport::Stdio { command, args, env } => {
            let args = if args.is_empty() {
                "(none)".to_string()
            } else {
                args.join(" ")
            };
            let env_keys = if env.is_empty() {
                "(none)".to_string()
            } else {
                env.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            let sensitive = mcp_sensitive_env_keys(cfg);
            let sensitive = if sensitive.is_empty() {
                "(none)".to_string()
            } else {
                sensitive.join(", ")
            };
            format!(
                "Command: {command}\nArgs: {args}\nEnv: {env_keys} (values masked)\nSensitive env: {sensitive}"
            )
        }
        agent_mcp::McpTransport::Http {
            url,
            bearer_token_env_var,
        } => {
            // The NAME of the variable is shown so the user knows which credential
            // this connection would hand over; its value is never read here.
            let token = bearer_token_env_var.as_deref().unwrap_or("(none)");
            format!("Endpoint: {url}\nBearer token from: {token}")
        }
    };
    let shadow = if cfg.shadows_lower_priority {
        "\nShadowing: hides a lower-priority MCP config."
    } else {
        ""
    };
    format!(
        "MCP \"{server}\": {lead}\nSource: {}\nTransport: {}\n{detail}{shadow}",
        cfg.source.display(),
        cfg.transport.short_label(),
    )
}

/// Projects the MCP registry into display metadata for the `/mcp` submenu.
fn mcp_metas(mcp: &Arc<Mutex<agent_mcp::McpRegistry>>) -> Vec<McpServerMeta> {
    let Ok(reg) = mcp.lock() else {
        return Vec::new();
    };
    reg.iter()
        .map(|(name, server)| McpServerMeta {
            name: name.clone(),
            status: match server {
                agent_mcp::McpServer::Disconnected { .. } => McpStatus::Disconnected,
                agent_mcp::McpServer::Connecting { .. } => McpStatus::Connecting,
                agent_mcp::McpServer::Connected { .. } => McpStatus::Connected,
                agent_mcp::McpServer::Failed { .. } => McpStatus::Failed,
            },
            source: server.config().source.short_label().to_string(),
            needs_trust: mcp_requires_trust(server.config()),
            tool_count: server.tool_count(),
        })
        .collect()
}

/// Loads the resumable sessions (8 most recent) as menu items,
/// excluding the current session. The label = 1st line of the 1st message.
fn load_sessions(dir: &Path, exclude: &Path) -> Vec<SessionMeta> {
    agent_session::list_sessions(dir, Some(exclude))
        .into_iter()
        .take(8)
        .map(|s| {
            let label = match s.summary.lines().next().map(str::trim) {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => "(untitled)".to_string(),
            };
            SessionMeta {
                id: s.id,
                label,
                hint: format!("{} msg · {}", s.message_count, relative_time(s.modified)),
            }
        })
        .collect()
}

fn workspace_file_mentions(root: &Path, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    collect_workspace_file_mentions(root, root, cap, &mut out);
    out.sort();
    out
}

fn collect_workspace_file_mentions(root: &Path, dir: &Path, cap: usize, out: &mut Vec<String>) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= cap {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
            if matches!(
                name.as_ref(),
                ".git" | "target" | "node_modules" | ".next" | "dist" | "build"
            ) {
                continue;
            }
            collect_workspace_file_mentions(root, &path, cap, out);
        } else if entry.file_type().map(|ty| ty.is_file()).unwrap_or(false)
            && is_mentionable_file_name(&name)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn is_mentionable_file_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        return false;
    }
    !matches!(
        Path::new(&lower).extension().and_then(|ext| ext.to_str()),
        Some("pem" | "key" | "p12" | "pfx")
    )
}

/// Human-readable session age.
fn relative_time(modified: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Path of a new session file (timestamped, one per conversation).
pub(crate) fn new_session_path(dir: &Path) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for seq in 0..1000 {
        let name = if seq == 0 {
            format!("{millis}.jsonl")
        } else {
            format!("{millis}-{seq}.jsonl")
        };
        let path = dir.join(name);
        if !path.exists() {
            return path;
        }
    }
    dir.join(format!("{millis}-overflow.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPBOARD_HELPERS, GOAL_DONE_MARKER, INIT_PROMPT, InitDecision, LOGOUT_SERVER_NOTE,
        apply_runtime_status, approvals_report, clipboard_failure, compose_system,
        count_encrypted_reasoning, hooks_report, init_decision, is_running, last_assistant_text,
        mcp_requires_trust, parse_turn_argument, runtime_facts, scrub_encrypted_reasoning,
        session_path_from_arg, take_goal_done, workspace_file_mentions,
    };
    use agent_core::message::{ContentBlock, Message};
    use agent_runtime::lifecycle::TurnState;
    use agent_runtime::thread::{ThreadStatus, TurnStatus};
    use agent_tui::{AppState, Block};
    use std::path::Path;
    use std::time::SystemTime;

    /// US-017 AC7: the legacy orchestration is GONE, not merely unused. A static
    /// search is the acceptance criterion itself, so it is the test: leaving two
    /// orchestrators alive is the epic's second risk, and a dormant one is still
    /// a second one.
    #[test]
    fn no_client_side_turn_orchestration_survives_in_this_loop() {
        let source = include_str!("interactive.rs");
        // The needles are split so this very assertion does not match itself.
        for banned in [
            concat!("Active", "Turn"),
            concat!("queued_", "prompts"),
            concat!("Queued", "Prompt"),
            concat!("launch_", "turn"),
            concat!("JoinHandle", "<"),
            concat!("handle", ".abort()"),
        ] {
            assert!(
                !source.contains(banned),
                "`{banned}` is back in the interactive loop: the runtime owns the turn lifecycle"
            );
        }
        // And what replaced it is really what the loop drives.
        assert!(source.contains("runtime.steer("));
        assert!(source.contains("runtime.interrupt("));
        assert!(source.contains("runtime.fork("));
        assert!(source.contains("runtime.shutdown()"));
    }

    /// US-017 AC5: the frontend reads its thread, turn, state and queue depth
    /// from the runtime's last-state signal.
    #[test]
    fn the_frontend_mirrors_the_runtime_state() {
        let thread_id = agent_runtime::ThreadId::generate(&agent_runtime::RandomIds);
        let turn_id = agent_runtime::TurnId::generate(&agent_runtime::RandomIds);
        let mut state = AppState::new("gpt-5", false);

        let idle = ThreadStatus {
            thread_id,
            health: agent_runtime::ThreadHealth::Healthy,
            turn: None,
            pending_inputs: 0,
            pending_steers: 0,
            shutting_down: false,
        };
        apply_runtime_status(&mut state, &idle);
        assert_eq!(state.thread_id, thread_id.to_string());
        assert_eq!(state.turn_id, None);
        assert_eq!(state.pending_inputs, 0);
        assert!(!is_running(&idle), "a thread without a turn is not running");

        let busy = ThreadStatus {
            turn: Some(TurnStatus {
                turn_id,
                state: TurnState::Running,
            }),
            // One input waiting for a turn of its own, two steering the running
            // turn: what the user typed and what is not consumed yet.
            pending_inputs: 1,
            pending_steers: 2,
            ..idle
        };
        apply_runtime_status(&mut state, &busy);
        assert_eq!(state.turn_id.as_deref(), Some(turn_id.to_string().as_str()));
        assert_eq!(state.turn_state.as_deref(), Some("running"));
        assert_eq!(state.pending_inputs, 3);
        assert!(is_running(&busy));

        let done = ThreadStatus {
            turn: Some(TurnStatus {
                turn_id,
                state: TurnState::Completed,
            }),
            ..busy
        };
        assert!(!is_running(&done), "a terminal turn is not running");
    }

    /// US-017 AC3: a malformed `<turn-id>` is refused before the runtime is
    /// asked for anything, and an empty argument means "the last boundary".
    #[test]
    fn a_branch_argument_is_a_turn_id_or_nothing() {
        assert_eq!(parse_turn_argument("   ").unwrap(), None);
        let turn_id = agent_runtime::TurnId::generate(&agent_runtime::RandomIds);
        assert_eq!(
            parse_turn_argument(&turn_id.to_string()).unwrap(),
            Some(turn_id)
        );
        // A thread identifier is not a turn identifier: the prefix is what says so.
        let thread_id = agent_runtime::ThreadId::generate(&agent_runtime::RandomIds);
        assert!(parse_turn_argument(&thread_id.to_string()).is_err());
        assert!(parse_turn_argument("nope").is_err());
    }

    /// US-019 AC3: `/status` reports the v1 bounds from the runtime CONSTANTS.
    /// FR-20 forbids a configuration key for them, so a value read from anywhere
    /// else would be describing a knob that does not exist.
    #[test]
    fn the_reported_limits_are_the_runtime_constants() {
        let facts = runtime_facts();
        assert_eq!(facts.max_active_agents, agent_runtime::MAX_ACTIVE_AGENTS);
        assert_eq!(
            facts.max_agents_per_root,
            agent_runtime::MAX_AGENTS_PER_ROOT
        );
        assert_eq!(facts.max_agent_depth, agent_runtime::MAX_AGENT_DEPTH);
        assert_eq!(facts.command_mailbox, agent_runtime::COMMAND_MAILBOX);
        assert_eq!(facts.max_pending_inputs, agent_runtime::MAX_PENDING_INPUTS);
    }

    /// US-013 AC5: the startup connection (US-012) and `/mcp connect` share this
    /// gate, so a repository-controlled declaration never earns a spawn on its own.
    #[test]
    fn a_workspace_controlled_server_stays_behind_the_trust_gate() {
        use agent_mcp::{
            McpConfigOrigin, McpConfigSource, McpServerConfig, McpToolPolicy, McpTransport,
        };
        use std::collections::BTreeMap;

        let server = |origin: McpConfigOrigin, shadows: bool, env: BTreeMap<String, String>| {
            McpServerConfig {
                transport: McpTransport::Stdio {
                    command: "srv".into(),
                    args: Vec::new(),
                    env,
                },
                tools: McpToolPolicy::default(),
                source: McpConfigSource::new(origin, ""),
                shadows_lower_priority: shadows,
            }
        };

        // User-scope, no shadowing, no sensitive env: connected at startup.
        assert!(!mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            false,
            BTreeMap::new()
        )));
        // Declared by the workspace: never auto-connected.
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::Workspace,
            false,
            BTreeMap::new()
        )));
        // Hides a user entry: same treatment.
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            true,
            BTreeMap::new()
        )));
        // Carries an env key that changes what gets executed.
        let mut env = BTreeMap::new();
        env.insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            false,
            env
        )));

        // US-013 AC4: the transport changes nothing. A remote server declared by
        // the workspace stays behind the same gate, and handing a credential to a
        // remote endpoint is exactly what must not happen on `cd` alone.
        let remote = |origin: McpConfigOrigin| McpServerConfig {
            transport: McpTransport::Http {
                url: "https://mcp.example.com/mcp".into(),
                bearer_token_env_var: Some("EXAMPLE_TOKEN".into()),
            },
            tools: McpToolPolicy::default(),
            source: McpConfigSource::new(origin, ""),
            shadows_lower_priority: false,
        };
        assert!(mcp_requires_trust(&remote(McpConfigOrigin::Workspace)));
        assert!(!mcp_requires_trust(&remote(McpConfigOrigin::ClaudeUser)));
    }

    #[test]
    fn approvals_report_lists_sequences_and_says_they_are_not_persisted() {
        // US-009 AC3: the user can check exactly what was remembered.
        use agent_tools::permission::ApprovalEntry;
        let empty = approvals_report(&[]);
        assert!(empty.contains("No answer remembered"), "{empty}");
        assert!(empty.contains("never persisted"), "{empty}");

        let report = approvals_report(&[
            ApprovalEntry {
                tool: "bash".into(),
                command: "git status".into(),
                allow: true,
            },
            ApprovalEntry {
                tool: "bash".into(),
                command: "rm -rf target".into(),
                allow: false,
            },
        ]);
        assert!(report.contains("allow  bash git status"), "{report}");
        assert!(report.contains("deny  bash rm -rf target"), "{report}");
        assert!(report.contains("/approvals clear"), "{report}");
    }

    #[test]
    fn compose_system_pins_completion_directive() {
        let base = "You are Pyxis.";
        assert_eq!(compose_system(base, None), base);
        assert_eq!(
            compose_system(base, Some("   ")),
            base,
            "empty goal leaves base unchanged"
        );
        let with = compose_system(base, Some("refactor the UI"));
        assert!(with.starts_with(base));
        assert!(
            with.contains("DO NOT STOP"),
            "completion directive should be present"
        );
        assert!(
            with.contains(GOAL_DONE_MARKER),
            "marker should be instructed"
        );
        assert!(with.contains("refactor the UI"));
    }

    #[test]
    fn workspace_file_mentions_skips_secret_filenames() {
        let root = std::env::temp_dir().join(format!(
            "pyxis-file-mentions-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("private.pem"), "").unwrap();

        let files = workspace_file_mentions(&root, 20);

        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(!files.iter().any(|path| path.contains(".env")));
        assert!(!files.iter().any(|path| path.contains("private.pem")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn take_goal_done_detects_and_strips_marker() {
        let mut s = AppState::new("gpt-5", false);
        // No marker: goal is not complete.
        s.blocks.push(Block::Assistant {
            text: "I started".into(),
            streaming: false,
        });
        assert!(!take_goal_done(&mut s));
        // Marker present: complete and stripped from display.
        s.blocks.push(Block::Assistant {
            text: format!("done\n{GOAL_DONE_MARKER}"),
            streaming: false,
        });
        assert!(take_goal_done(&mut s));
        assert!(
            matches!(s.blocks.last(), Some(Block::Assistant { text, .. })
                if text == "done" && !text.contains(GOAL_DONE_MARKER)),
            "marker should be stripped from the last block",
        );
    }

    #[test]
    fn take_goal_done_requires_marker_as_last_line() {
        let mut s = AppState::new("gpt-5", false);
        s.blocks.push(Block::Assistant {
            text: format!("text {GOAL_DONE_MARKER} in the middle"),
            streaming: false,
        });
        assert!(!take_goal_done(&mut s));

        s.blocks.push(Block::Assistant {
            text: format!("done\n{GOAL_DONE_MARKER}\n\n"),
            streaming: false,
        });
        assert!(take_goal_done(&mut s));
    }

    #[test]
    fn session_path_from_arg_rejects_path_traversal() {
        let sessions = Path::new("/tmp/pyxis-sessions");
        assert_eq!(
            session_path_from_arg(sessions, "123.jsonl").unwrap(),
            sessions.join("123.jsonl")
        );
        assert!(session_path_from_arg(sessions, "../123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "/tmp/123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "nested/123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "123.txt").is_none());
    }

    fn tmp_workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis-us019-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// US-019 AC1/AC2: with nothing to protect the bootstrap turn starts; with an
    /// instruction file already there it does NOT, and the file is named.
    #[test]
    fn init_refuses_to_overwrite_without_an_explicit_force() {
        let ws = tmp_workspace("init");
        assert_eq!(init_decision(&ws, ""), InitDecision::Bootstrap);
        assert_eq!(init_decision(&ws, "force"), InitDecision::Bootstrap);

        std::fs::write(ws.join("AGENTS.md"), "rules").unwrap();
        assert_eq!(init_decision(&ws, ""), InitDecision::Confirm("AGENTS.md"));
        // Anything that is not the confirmation keeps refusing.
        assert_eq!(
            init_decision(&ws, "yes"),
            InitDecision::Confirm("AGENTS.md")
        );
        assert_eq!(
            init_decision(&ws, " force "),
            InitDecision::Overwrite("AGENTS.md")
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// US-019 AC2: the tolerated `CLAUDE.md` fallback is protected the same way,
    /// and so is a dangling symlink, which `exists()` would have declared absent.
    #[test]
    fn init_protects_every_instruction_file_shape() {
        let ws = tmp_workspace("init-shapes");
        std::fs::write(ws.join("CLAUDE.md"), "rules").unwrap();
        assert_eq!(init_decision(&ws, ""), InitDecision::Confirm("CLAUDE.md"));
        let _ = std::fs::remove_dir_all(&ws);

        #[cfg(unix)]
        {
            let ws = tmp_workspace("init-dangling");
            std::os::unix::fs::symlink(ws.join("gone.md"), ws.join("AGENTS.md")).unwrap();
            assert_eq!(init_decision(&ws, ""), InitDecision::Confirm("AGENTS.md"));
            let _ = std::fs::remove_dir_all(&ws);
        }
    }

    /// US-019 AC4: the LAST answer, raw, with the goal marker removed like the
    /// headless output does. Nothing to copy is not an empty copy.
    #[test]
    fn copy_takes_the_last_answer_raw() {
        let mut state = AppState::new("gpt-5", false);
        assert_eq!(last_assistant_text(&state), None);

        state.blocks.push(Block::Assistant {
            text: "first".into(),
            streaming: false,
        });
        state.blocks.push(Block::Assistant {
            text: format!("**bold** answer\n{GOAL_DONE_MARKER}\n"),
            streaming: false,
        });
        state.blocks.push(Block::Notice("a notice".into()));
        assert_eq!(
            last_assistant_text(&state).as_deref(),
            Some("**bold** answer"),
            "markup kept as streamed, marker removed"
        );
    }

    /// US-019 AC4: a clipboard that cannot be reached is NAMED. The message says
    /// what was tried, otherwise "no clipboard" is unactionable.
    #[test]
    fn a_clipboard_failure_names_what_was_tried() {
        let message = clipboard_failure(&[]);
        for (program, _) in CLIPBOARD_HELPERS {
            assert!(message.contains(program), "message: {message}");
        }
        let detailed = clipboard_failure(&["wl-copy: exit 1".to_string()]);
        assert!(detailed.contains("wl-copy: exit 1"), "message: {detailed}");
    }

    /// US-019 AC1: what the bootstrap turn asks for. Pinned because the ONLY
    /// thing that makes the written `AGENTS.md` describe THIS repository rather
    /// than a plausible one is that the prompt demands a real inspection.
    #[test]
    fn the_init_prompt_demands_an_inspection_and_names_the_file() {
        assert!(INIT_PROMPT.contains("AGENTS.md"), "{INIT_PROMPT}");
        assert!(INIT_PROMPT.contains("for real"), "{INIT_PROMPT}");
        assert!(
            INIT_PROMPT.contains("Never state a command you have not seen declared"),
            "{INIT_PROMPT}"
        );
    }

    /// US-019 AC5: signing out deletes a LOCAL credential and nothing else. The
    /// sentence saying so is pinned, because dropping it would leave the user
    /// believing the ChatGPT session is closed when it is not.
    #[test]
    fn the_sign_out_message_states_the_absence_of_server_revocation() {
        assert!(LOGOUT_SERVER_NOTE.contains("NOT revoked server-side"));
        assert!(LOGOUT_SERVER_NOTE.contains("local credential"));
        assert!(LOGOUT_SERVER_NOTE.contains("OpenAI account"));
    }

    /// US-019 AC6: every declared hook is listed with its event and its matcher.
    /// A lifecycle hook names no tool, and that is shown as `*` rather than left
    /// blank, which would read as an unknown.
    #[test]
    fn hooks_are_listed_with_event_and_matcher() {
        use agent_tools::hooks::{HookEvent, HookSpec};

        assert!(hooks_report(&[]).contains("No hook declared"));

        let report = hooks_report(&[
            HookSpec {
                event: HookEvent::PreToolUse,
                matcher: Some("Bash".into()),
                command: "guard".into(),
                args: vec!["--strict".into()],
            },
            HookSpec {
                event: HookEvent::SessionStart,
                matcher: None,
                command: "notify".into(),
                args: Vec::new(),
            },
        ]);
        assert!(report.contains("2 hook(s) declared"), "{report}");
        assert!(report.contains("PreToolUse"), "{report}");
        assert!(report.contains("matcher=Bash"), "{report}");
        assert!(report.contains("guard --strict"), "{report}");
        assert!(report.contains("SessionStart"), "{report}");
        // A lifecycle event names no tool: `-`, not `*` which would read as
        // "every tool".
        assert!(report.contains("matcher=-"), "{report}");

        let every_tool = hooks_report(&[HookSpec {
            event: HookEvent::PostToolUse,
            matcher: None,
            command: "audit".into(),
            args: Vec::new(),
        }]);
        assert!(every_tool.contains("matcher=*"), "{every_tool}");
    }

    #[test]
    fn scrub_encrypted_reasoning_removes_only_replay_blocks() {
        let mut messages = vec![Message::assistant(vec![
            ContentBlock::Text { text: "ok".into() },
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into(),
            },
            ContentBlock::tool_use("c1", "bash", serde_json::json!({})),
        ])];
        assert_eq!(count_encrypted_reasoning(&messages), 1);
        assert_eq!(scrub_encrypted_reasoning(&mut messages), 1);
        assert_eq!(count_encrypted_reasoning(&messages), 0);
        assert!(
            messages[0]
                .content
                .iter()
                .all(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }))
        );
        assert_eq!(messages[0].content.len(), 2);
    }
}
