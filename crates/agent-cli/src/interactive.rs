//! Interactive loop: assembles the frontend (`agent-tui`), the agent stream
//! (`agent-core`) and the permission requests into a single `tokio::select`.
//!
//! - Keystrokes arrive from a dedicated thread (crossterm `read()` blocks).
//! - Each submission spawns `run_agent`; its `AgentEvent` come back through an mpsc.
//! - A permission request suspends the tool pipeline until the user
//!   answers (the dialog does NOT freeze the loop: the select keeps rendering
//!   and reading the keyboard).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_core::message::{ContentBlock, Message, recent_untrusted_content};
use agent_core::provider::ToolSpec;
use agent_core::{AgentContext, AgentEvent, CancelToken, Deps, RunConfig, Session, run_agent};
use agent_provider::KEYRING_ACCOUNT;
use agent_tools::PermissionModeState;
use agent_tui::{
    AppState, Block, COMMANDS, InputAction, McpServerMeta, McpStatus, SessionMeta,
    blocks_from_messages, default_reasoning_effort_for_model, normalize_reasoning_effort_for_model,
    permission_mode_label, reasoning_effort_label, supported_reasoning_efforts_for_model,
};
#[cfg(feature = "codex_tui_parity")]
use agent_tui::{
    BottomPane, ChatSurface, HistoryInserter, InsertHistoryMode, PermissionTranscriptRequest,
    TerminalViewport, TerminalViewportState, TranscriptMapper,
};
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::approver::{PermissionMsg, to_prompt};
use crate::session::SharedSession;
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

struct AgentTurnEvent {
    turn_id: u64,
    event: AgentEvent,
}

struct ActiveTurn {
    next_id: u64,
    id: Option<u64>,
    handle: Option<JoinHandle<()>>,
    /// US-001: cancellation signal of the current turn, one per turn.
    cancel: Option<CancelToken>,
}

impl ActiveTurn {
    fn new() -> Self {
        Self {
            next_id: 1,
            id: None,
            handle: None,
            cancel: None,
        }
    }

    fn start(
        &mut self,
        conversation: &Arc<Mutex<Vec<Message>>>,
        cfg: &InteractiveConfig,
        deps: &Deps,
        tx: &mpsc::Sender<AgentTurnEvent>,
        user_msg: &str,
        persist_user_message: bool,
    ) {
        self.abort();
        let turn_id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.id = Some(turn_id);
        // US-001: every turn carries ITS signal, so that a cancellation cannot
        // reach the next turn.
        let cancel = CancelToken::new();
        let mut deps = deps.clone();
        deps.cancel = cancel.clone();
        self.handle = Some(launch_turn(
            conversation,
            cfg,
            &deps,
            tx,
            turn_id,
            user_msg,
            persist_user_message,
        ));
        self.cancel = Some(cancel);
    }

    fn is_current(&self, turn_id: u64) -> bool {
        self.id == Some(turn_id)
    }

    fn finish(&mut self) {
        self.handle.take();
        self.cancel.take();
        self.id = None;
    }

    /// US-001: requests the COOPERATIVE stop of the turn. The core loop reconciles
    /// its in-flight tool calls, persists, then emits `Interrupted` itself:
    /// that event is what closes the turn on the client side, not this call.
    fn request_cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    fn abort(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        self.cancel.take();
        self.id = None;
    }
}

pub struct InteractiveConfig {
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Behavioral guidelines of the tools (US-026), injected into the system
    /// prompt. Stored raw (not pre-composed) because the base system depends on the
    /// current slug (US-027) and is recomposed per turn.
    pub tool_guidelines: Vec<String>,
    /// Ephemeral project context (AGENTS.md + env, US-028), re-injected on every turn
    /// into `AgentContext::context_messages` (never persisted).
    pub context_messages: Vec<Message>,
    pub run_config: RunConfig,
    pub tool_specs: Vec<ToolSpec>,
    pub truecolor: bool,
    /// Reduced motion (`NO_COLOR` / `PYXIS_REDUCED_MOTION`): spinner degraded to a
    /// pulsing dot rather than animated (US-044).
    pub reduced_motion: bool,
    /// Provider credential present (connected badge + providers submenu).
    pub connected: bool,
    /// Available skills (read before the sandbox), `/skills` submenu.
    pub skills: Vec<String>,
    /// Persistent session goal (`/goal`), composed into the system prompt on every
    /// turn. Loaded from the session sidecar at startup.
    pub goal: Option<String>,
    /// Hardening applied to the MCP subprocesses (env scrub + proxy).
    pub command_hardener: agent_tools::CommandHardener,
    /// Mutable permission mode, shared with the tool registry.
    pub permission_mode: PermissionModeState,
    /// Global user settings, used to persist the interactive choices.
    pub settings_path: Option<PathBuf>,
    /// Workspace root, scope of the aggregated turn diff (US-018).
    pub workspace: PathBuf,
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

const MCP_DISABLED_NOTICE: &str =
    "MCP: config diagnostics only. MCP tool execution is not exposed in this build.";

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

/// Builds the turn context (up-to-date conversation + message) and launches
/// `run_agent` in a task whose events come back through `tx`.
fn launch_turn(
    conversation: &Arc<Mutex<Vec<Message>>>,
    cfg: &InteractiveConfig,
    deps: &Deps,
    tx: &mpsc::Sender<AgentTurnEvent>,
    turn_id: u64,
    user_msg: &str,
    persist_user_message: bool,
) -> JoinHandle<()> {
    let mut msgs = conversation.lock().map(|g| g.clone()).unwrap_or_default();
    let ephemeral_messages = if persist_user_message {
        msgs.push(Message::user(user_msg.to_string()));
        Vec::new()
    } else {
        vec![Message::user(user_msg.to_string())]
    };
    // US-027: base system prompt selected by the CURRENT slug (recomputed per turn ->
    // a `/models` changes the template) + tool guidelines + goal directive.
    let base = with_tool_guidelines(
        crate::prompt::select_system_prompt(&cfg.model),
        &cfg.tool_guidelines,
    );
    let ctx = AgentContext {
        model: cfg.model.clone(),
        reasoning_effort: cfg.reasoning_effort.clone(),
        system: Some(compose_system(&base, cfg.goal.as_deref())),
        messages: msgs,
        tools: cfg.tool_specs.clone(),
        config: cfg.run_config.clone(),
        // US-028: project context re-injected every turn, never persisted.
        context_messages: cfg.context_messages.clone(),
        ephemeral_messages,
    };
    // The received `Deps` already carries the turn cancellation signal (`ActiveTurn::start`).
    let deps = deps.clone();
    let tx = tx.clone();
    let workspace = cfg.workspace.clone();
    tokio::spawn(async move {
        // US-018: diff reference taken before the first round-trip.
        let mut diff_tracker = agent_tools::turn_diff::TurnDiffTracker::begin(&workspace).await;
        let stream = run_agent(ctx, deps);
        futures_util::pin_mut!(stream);
        // The terminal event is held back long enough to compute the diff: the
        // interface loop closes the turn as soon as it sees it, and anything
        // arriving afterwards would be discarded by the `turn_id` filter.
        let mut terminal: Option<AgentEvent> = None;
        while let Some(ev) = stream.next().await {
            if matches!(
                ev,
                AgentEvent::EndTurn
                    | AgentEvent::Interrupted
                    | AgentEvent::Error(_)
                    | AgentEvent::Exhausted(_)
            ) {
                terminal = Some(ev);
                break;
            }
            if tx
                .send(AgentTurnEvent { turn_id, event: ev })
                .await
                .is_err()
            {
                return;
            }
        }
        if let Some(terminal) = terminal {
            match diff_tracker.turn_diff().await {
                Ok(diff) if !diff.is_empty() => {
                    if tx
                        .send(AgentTurnEvent {
                            turn_id,
                            event: AgentEvent::TurnDiff(diff),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(_) => {}
                Err(err) => agent_tui::debug_log::log(&format!("turn diff: {err}")),
            }
            let _ = tx
                .send(AgentTurnEvent {
                    turn_id,
                    event: terminal,
                })
                .await;
        }
    })
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

fn show_shutdown_feedback(
    state: &mut AppState,
    active_turn: &mut ActiveTurn,
    pending_resp: &mut Option<oneshot::Sender<bool>>,
    running: &mut bool,
    turn_start: &mut Option<Instant>,
) {
    if let Some(resp) = pending_resp.take() {
        let _ = resp.send(false);
    }
    active_turn.abort();
    *running = false;
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

/// Starts the interactive session. Restores the terminal on exit whatever happens.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    deps: Deps,
    conversation: Arc<Mutex<Vec<Message>>>,
    perm_rx: mpsc::Receiver<PermissionMsg>,
    cfg: InteractiveConfig,
    session: Arc<SharedSession>,
    sessions_dir: PathBuf,
    current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
) -> anyhow::Result<()> {
    let mut tui = agent_tui::enter()?;
    let result = event_loop(
        &mut tui,
        deps,
        conversation,
        perm_rx,
        cfg,
        session,
        sessions_dir,
        current_session,
        mcp,
    )
    .await;
    let clear_result = agent_tui::clear(&mut tui);
    agent_tui::leave(&mut tui)?;
    clear_result?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    tui: &mut agent_tui::Tui,
    deps: Deps,
    conversation: Arc<Mutex<Vec<Message>>>,
    mut perm_rx: mpsc::Receiver<PermissionMsg>,
    mut cfg: InteractiveConfig,
    session: Arc<SharedSession>,
    sessions_dir: PathBuf,
    mut current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
) -> anyhow::Result<()> {
    let mut state = AppState::new(cfg.model.clone(), cfg.truecolor);
    state.set_permission_mode(permission_mode_id(cfg.permission_mode.get()));
    state.workspace = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    state.reasoning_effort = cfg.reasoning_effort.clone();
    state.provider_connected = cfg.connected;
    state.reduced_motion = cfg.reduced_motion;
    state.skills = std::mem::take(&mut cfg.skills);
    state.files = std::env::current_dir()
        .ok()
        .map(|root| workspace_file_mentions(&root, 200))
        .unwrap_or_default();
    state.sessions = load_sessions(&sessions_dir, &current_session);
    state.mcp_servers = mcp_metas(&mcp);
    let mut goal_path = goal_path_for_session(&current_session);
    let mut goal_iters_path = goal_iters_path_for_session(&current_session);
    // Prompt history of the WHOLE directory (every conversation).
    state.load_history(agent_session::workspace_prompts(
        &sessions_dir,
        Some(&current_session),
        PROMPT_HISTORY_CAP,
    ));
    let initial_messages = conversation.lock().map(|g| g.clone()).unwrap_or_default();
    if !initial_messages.is_empty() {
        state.blocks = blocks_from_messages(&initial_messages);
        state.blocks.push(Block::Notice(format!(
            "Session resumed ({} messages).",
            initial_messages.len()
        )));
    }
    #[cfg(feature = "codex_tui_parity")]
    let mut parity_mapper = TranscriptMapper::new();
    #[cfg(feature = "codex_tui_parity")]
    let mut parity_surface = ChatSurface::from_messages(&initial_messages);
    #[cfg(feature = "codex_tui_parity")]
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
    #[cfg(feature = "codex_tui_parity")]
    let mut parity_bottom_pane = BottomPane::new();
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

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentTurnEvent>(256);
    let (mcp_tx, mut mcp_rx) = mpsc::channel::<McpEvent>(16);
    let mut running = false;
    let mut active_turn = ActiveTurn::new();
    // Counter of automatic re-prompts of the goal loop (reset on every
    // user input / new goal).
    let mut goal_iters: u32 = if cfg.goal.is_some() {
        read_goal_iters(&goal_iters_path)
    } else {
        0
    };
    let mut pending_resp: Option<oneshot::Sender<bool>> = None;
    let mut queued_prompts: VecDeque<String> = VecDeque::new();

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
                && let Some(insert) =
                    parity_surface.drain_pending_insert(size.width, parity_inserter.mode())
                && let Err(err) = parity_inserter.insert(tui, &insert)
            {
                parity_viewport.activate_legacy_fallback(err.message().to_string());
                state.blocks.push(Block::Notice(err.message().to_string()));
            }
        }
        #[cfg(feature = "codex_tui_parity")]
        {
            if parity_inserter.mode() == InsertHistoryMode::InlineScrollback {
                tui.draw(|f| agent_tui::render_parity(f, &state, &parity_surface))?;
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
                            parity_bottom_pane.route_paste(&mut state, &p);
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
                let action = parity_bottom_pane.route_key(&mut state, k);
                #[cfg(not(feature = "codex_tui_parity"))]
                let action = state.on_key(k);
                match action {
                    InputAction::Submit(prompt) if !running => {
                        state.push_user(prompt.clone());
                        #[cfg(feature = "codex_tui_parity")]
                        parity_surface.apply_update(parity_mapper.map_user_message(prompt.clone()));
                        goal_iters = 0;
                        active_turn.start(
                            &conversation,
                            &cfg,
                            &deps,
                            &agent_tx,
                            &prompt,
                            true,
                        );
                        running = true;
                    }
                    InputAction::Submit(prompt) => {
                        state.push_user(prompt.clone());
                        #[cfg(feature = "codex_tui_parity")]
                        parity_surface.apply_update(parity_mapper.map_user_message(prompt.clone()));
                        state.blocks.push(Block::Notice(
                            "Message queued.".into(),
                        ));
                        queued_prompts.push_back(prompt);
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
                                    let removed = conversation
                                        .lock()
                                        .map(|msgs| count_encrypted_reasoning(&msgs[..]))
                                        .unwrap_or_default();
                                    if removed > 0
                                        && let Err(e) = session.redact_encrypted_reasoning().await
                                    {
                                        state.blocks.push(Block::Error(format!(
                                            "models: redaction reasoning: {e}"
                                        )));
                                        continue;
                                    }
                                    if removed > 0 {
                                        let _ = conversation
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
                                    #[cfg(feature = "codex_tui_parity")]
                                    parity_surface.apply_update(parity_mapper.map_notice(message));
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
                                    if let Err(e) = std::fs::write(&goal_path, g) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = write_goal_iters(&goal_iters_path, 0) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    goal_iters = 0;
                                    state.push_user(g);
                                    #[cfg(feature = "codex_tui_parity")]
                                    parity_surface
                                        .apply_update(parity_mapper.map_user_message(g.to_string()));
                                    active_turn.start(
                                        &conversation,
                                        &cfg,
                                        &deps,
                                        &agent_tx,
                                        g,
                                        true,
                                    );
                                    running = true;
                                }
                            },
                            // resume / new / clear during a turn: we wait (the
                            // persistence file is being written by the stream).
                            "/resume" | "/new" | "/clear" if running => {
                                state.blocks.push(Block::Notice(
                                    "Wait for the current turn to finish.".into(),
                                ));
                            }
                            "/resume" => {
                                let path = match crate::resolve_resume_path(&sessions_dir, arg) {
                                    Ok(path) => path,
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("{e}")));
                                        continue;
                                    }
                                };
                                match agent_session::resume_file(&path) {
                                    Ok(r) if !r.messages.is_empty() => {
                                        let msgs = r.messages;
                                        if let Err(e) = session.switch_file(&path, msgs.len()) {
                                            state.blocks.push(Block::Error(format!("resume: {e}")));
                                        } else {
                                            current_session = path;
                                            goal_path = goal_path_for_session(&current_session);
                                            goal_iters_path =
                                                goal_iters_path_for_session(&current_session);
                                            cfg.goal = read_goal(&goal_path);
                                            goal_iters = if cfg.goal.is_some() {
                                                read_goal_iters(&goal_iters_path)
                                            } else {
                                                0
                                            };
                                            deps.provider.set_prompt_cache_key(
                                                &prompt_cache_key_for_session(&current_session),
                                            );
                                            if let Ok(mut g) = conversation.lock() {
                                                *g = msgs.clone();
                                            }
                                            deps.tools.seed_taint(recent_untrusted_content(
                                                &msgs,
                                                crate::RESUME_TAINT_SCAN_MESSAGES,
                                            ));
                                            state.blocks = blocks_from_messages(&msgs);
                                            #[cfg(feature = "codex_tui_parity")]
                                            {
                                                parity_mapper = TranscriptMapper::new();
                                                parity_surface = ChatSurface::from_messages(&msgs);
                                            }
                                            // Prompt history stays folder-wide.
                                            state.blocks.push(Block::Notice(format!(
                                                "Session resumed ({} messages).",
                                                msgs.len()
                                            )));
                                            state.sessions =
                                                load_sessions(&sessions_dir, &current_session);
                                        }
                                    }
                                    Ok(_) => {
                                        if let Err(e) = session.switch_file(&path, 0) {
                                            state.blocks.push(Block::Error(format!("resume: {e}")));
                                            continue;
                                        }
                                        current_session = path;
                                        goal_path = goal_path_for_session(&current_session);
                                        goal_iters_path =
                                            goal_iters_path_for_session(&current_session);
                                        cfg.goal = read_goal(&goal_path);
                                        goal_iters = if cfg.goal.is_some() {
                                            read_goal_iters(&goal_iters_path)
                                        } else {
                                            0
                                        };
                                        deps.provider.set_prompt_cache_key(
                                            &prompt_cache_key_for_session(&current_session),
                                        );
                                        if let Ok(mut g) = conversation.lock() {
                                            g.clear();
                                        }
                                        state.blocks.clear();
                                        state
                                            .blocks
                                            .push(Block::Notice("Empty session.".into()));
                                        #[cfg(feature = "codex_tui_parity")]
                                        {
                                            parity_mapper = TranscriptMapper::new();
                                            parity_surface = ChatSurface::new();
                                        }
                                        state.sessions =
                                            load_sessions(&sessions_dir, &current_session);
                                    }
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("resume: {e}")))
                                    }
                                }
                            }
                            // /clear is an alias of /new: same mechanics (new
                            // session file + cleared context), only the label changes.
                            "/new" | "/clear" => {
                                let path = new_session_path(&sessions_dir);
                                if let Err(e) = session.switch_file(&path, 0) {
                                    state.blocks.push(Block::Error(format!("{cmd}: {e}")));
                                } else {
                                    current_session = path;
                                    goal_path = goal_path_for_session(&current_session);
                                    goal_iters_path = goal_iters_path_for_session(&current_session);
                                    cfg.goal = None;
                                    goal_iters = 0;
                                    if let Err(e) = remove_if_exists(&goal_path) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = remove_if_exists(&goal_iters_path) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    deps.provider.set_prompt_cache_key(
                                        &prompt_cache_key_for_session(&current_session),
                                    );
                                    if let Ok(mut g) = conversation.lock() {
                                        g.clear();
                                    }
                                    // Transcript cleared -> the welcome screen comes back,
                                    // which serves as visual confirmation (no Notice).
                                    state.blocks.clear();
                                    #[cfg(feature = "codex_tui_parity")]
                                    {
                                        parity_mapper = TranscriptMapper::new();
                                        parity_surface = ChatSurface::new();
                                    }
                                    state.sessions =
                                        load_sessions(&sessions_dir, &current_session);
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
                                "subscription codex disconnect" => {
                                    if state.provider_connected {
                                        if let Err(e) = agent_auth::store::delete(KEYRING_ACCOUNT) {
                                            state
                                                .blocks
                                                .push(Block::Error(format!("disconnect: {e}")));
                                        } else if let Err(e) = deps.provider.disconnect_auth().await {
                                            state.blocks.push(Block::Error(format!(
                                                "provider disconnect: {e}"
                                            )));
                                        } else {
                                                state.provider_connected = false;
                                                state.blocks.push(Block::Notice(
                                                    "Disconnected from Codex (credential removed). \
                                                     Log in again before the next model call."
                                                        .into(),
                                                ));
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
                                handle_mcp(arg, &mcp, &mcp_tx, &cfg.command_hardener, &mut state)
                            }
                            "/skills" => state.blocks.push(Block::Notice(
                                "Choose a skill in the /skills submenu.".into(),
                            )),
                            "/quit" => show_shutdown_feedback(
                                &mut state,
                                &mut active_turn,
                                &mut pending_resp,
                                &mut running,
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
                        &mut active_turn,
                        &mut pending_resp,
                        &mut running,
                        &mut turn_start,
                    ),
                    InputAction::Interrupt if running => {
                        if let Some(resp) = pending_resp.take() {
                            let _ = resp.send(false);
                        }
                        // US-001: no more brutal `abort()` nor fabricated `Interrupted`
                        // here. We signal, and the turn closes when the
                        // `Interrupted` emitted by the core arrives, transcript already reconciled
                        // (`stop` branch below, as for an EndTurn).
                        active_turn.request_cancel();
                    }
                    InputAction::Interrupt => {}
                    InputAction::Permission(allow) => {
                        if let Some(resp) = pending_resp.take() {
                            let _ = resp.send(allow);
                        }
                        #[cfg(feature = "codex_tui_parity")]
                        parity_surface.apply_update(parity_mapper.map_approval_decision(allow));
                    }
                    _ => {}
                }
            }
            ev = agent_rx.recv(), if running => {
                if let Some(turn_event) = ev {
                    if !active_turn.is_current(turn_event.turn_id) {
                        continue;
                    }
                    let ev = turn_event.event;
                    let endturn = matches!(ev, AgentEvent::EndTurn);
                    let stop = matches!(
                        ev,
                        AgentEvent::EndTurn
                            | AgentEvent::Interrupted
                            | AgentEvent::Error(_)
                            | AgentEvent::Exhausted(_)
                    );
                    // Calibration probe (US-002): in interactive mode the TUI owns
                    // the terminal, so the line goes to the debug log, never to a
                    // process output.
                    if let AgentEvent::ModelTurn(view) = &ev
                        && let Some(line) = crate::jsonl::usage_probe_line(view)
                    {
                        agent_tui::debug_log::log(&line);
                    }
                    state.apply(&ev);
                    #[cfg(feature = "codex_tui_parity")]
                    {
                        for update in parity_mapper.map_event(&ev) {
                            parity_surface.apply_update(update);
                        }
                    }
                    if stop {
                        active_turn.finish();
                        // Goal loop: on a "clean" EndTurn with an active
                        // goal, we re-prompt as long as the completion marker is
                        // not emitted (the model does not decide alone to stop).
                        if endturn && cfg.goal.is_some() && queued_prompts.is_empty() {
                            if take_goal_done(&mut state) {
                                cfg.goal = None;
                                if let Err(e) = remove_if_exists(&goal_path) {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                }
                                if let Err(e) = remove_if_exists(&goal_iters_path) {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                }
                                state
                                    .blocks
                                    .push(Block::Notice("Goal completed and cleared.".into()));
                                running = false;
                            } else if goal_iters < MAX_GOAL_ITERS {
                                goal_iters += 1;
                                if let Err(e) = write_goal_iters(&goal_iters_path, goal_iters) {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                    running = false;
                                    continue;
                                }
                                state.blocks.push(Block::Notice(format!(
                                    "Continuing goal ({goal_iters}/{MAX_GOAL_ITERS})..."
                                )));
                                turn_start = None;
                                state.end_turn();
                                active_turn.start(
                                    &conversation,
                                    &cfg,
                                    &deps,
                                    &agent_tx,
                                    GOAL_CONTINUE_PROMPT,
                                    false,
                                );
                                // running stays true: a new turn is launched.
                            } else {
                                state.blocks.push(Block::Notice(format!(
                                    "Goal not confirmed after {MAX_GOAL_ITERS} retries. \
                                     Use /goal clear to abandon it."
                                )));
                                running = false;
                            }
                        } else {
                            running = false;
                        }
                        if !running
                            && let Some(next) = queued_prompts.pop_front()
                        {
                            goal_iters = 0;
                            turn_start = None;
                            state.end_turn();
                            active_turn.start(
                                &conversation,
                                &cfg,
                                &deps,
                                &agent_tx,
                                &next,
                                true,
                            );
                            running = true;
                        }
                    }
                }
            }
            perm = perm_rx.recv() => {
                if let Some((req, resp)) = perm {
                    state.pending = Some(to_prompt(&req));
                    #[cfg(feature = "codex_tui_parity")]
                    {
                        for update in parity_mapper.map_permission_request(PermissionTranscriptRequest {
                            call_id: req.call_id.clone(),
                            tool: req.tool.clone(),
                            reason: req.reason.clone(),
                            taint_forced: req.taint_forced,
                            input_summary: req.input_summary.clone(),
                            mode: req.mode.clone(),
                            input: req.input.clone(),
                        }) {
                            parity_surface.apply_update(update);
                        }
                    }
                    pending_resp = Some(resp);
                }
            }
            ev = mcp_rx.recv() => {
                if let Some(ev) = ev {
                    match ev {
                        McpEvent::Connected { name, conn, tools } => {
                            let n = tools.len();
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
                                        state.blocks.push(Block::Notice(format!(
                                            "MCP \"{name}\" connected ({n} tools)."
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
    active_turn.abort();
    Ok(())
}

/// Handles `/mcp [<server> <action>]`. Connections (spawn + handshake) are
/// started in the background; the result comes back through `mcp_tx` into the loop.
fn handle_mcp(
    arg: &str,
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    mcp_tx: &mpsc::Sender<McpEvent>,
    command_hardener: &agent_tools::CommandHardener,
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
            if !mcp_connect_enabled() {
                state.blocks.push(Block::Notice(MCP_DISABLED_NOTICE.into()));
                return;
            }
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
            if !mcp_connect_enabled() {
                state.blocks.push(Block::Notice(MCP_DISABLED_NOTICE.into()));
                return;
            }
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
                    state
                        .blocks
                        .push(Block::Notice(format!("MCP \"{server}\" disconnected.")));
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

fn mcp_connect_enabled() -> bool {
    std::env::var_os("PYXIS_EXPERIMENTAL_MCP_CONNECT").is_some()
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
            if let Some(expected) = trusted_cfg
                && (expected.command != cfg_srv.command
                    || expected.args != cfg_srv.args
                    || expected.env != cfg_srv.env)
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

fn mcp_requires_trust(cfg: &agent_mcp::McpServerConfig) -> bool {
    matches!(cfg.source.origin, agent_mcp::McpConfigOrigin::Workspace)
        || cfg.shadows_lower_priority
        || !mcp_sensitive_env_keys(cfg).is_empty()
}

fn mcp_sensitive_env_keys(cfg: &agent_mcp::McpServerConfig) -> Vec<&str> {
    cfg.env
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
    let args = if cfg.args.is_empty() {
        "(none)".to_string()
    } else {
        cfg.args.join(" ")
    };
    let env_keys = if cfg.env.is_empty() {
        "(none)".to_string()
    } else {
        cfg.env.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    let sensitive = mcp_sensitive_env_keys(cfg);
    let sensitive = if sensitive.is_empty() {
        "(none)".to_string()
    } else {
        sensitive.join(", ")
    };
    let shadow = if cfg.shadows_lower_priority {
        "\nShadowing: hides a lower-priority MCP config."
    } else {
        ""
    };
    format!(
        "MCP \"{server}\": {lead}\nSource: {}\nCommand: {}\nArgs: {args}\nEnv: {env_keys} (values masked)\nSensitive env: {sensitive}{shadow}",
        cfg.source.display(),
        cfg.command
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
        GOAL_DONE_MARKER, compose_system, count_encrypted_reasoning, scrub_encrypted_reasoning,
        session_path_from_arg, take_goal_done, workspace_file_mentions,
    };
    use agent_core::message::{ContentBlock, Message};
    use agent_tui::{AppState, Block};
    use std::path::Path;
    use std::time::SystemTime;

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

    #[test]
    fn scrub_encrypted_reasoning_removes_only_replay_blocks() {
        let mut messages = vec![Message::assistant(vec![
            ContentBlock::Text { text: "ok".into() },
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
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
