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
//!
//! One structural rule shapes the module: the `select!` only NAMES what woke the
//! loop ([`Wake`]) and every handler runs afterwards on `&mut Loop`. Inlining the
//! handling in the branches is what forced twenty mutable locals to be threaded
//! by hand and made a slash command a 600-line arm; with the state in one place a
//! command is a method, `commands` and `mcp` are separate files, and a switch of
//! conversation is written once ([`Loop::switch_to`]) instead of copied per
//! command.

mod commands;
mod mcp;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_core::AgentEvent;
use agent_core::message::{Message, recent_untrusted_content};
use agent_core::provider::Provider;
use agent_provider::KEYRING_ACCOUNT;
use agent_runtime::lifecycle::TurnState;
use agent_runtime::thread::{
    RuntimeEvent, RuntimeEventPayload, Submission, SubmitError, ThreadStatus,
};
use agent_tools::PermissionModeState;
use agent_tui::{AppState, Block, InputAction, SessionMeta, blocks_from_messages};
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "codex_tui_parity")]
use agent_tui::{ChatWidget, HistoryInserter, InsertHistoryMode, PermissionTranscriptRequest};

use crate::approver::{PermissionMsg, to_prompt};
use crate::runtime::{CliStepSource, EngineDeps, SessionRuntime, SettingsCell};
use crate::settings::permission_mode_id;

use mcp::McpEvent;
pub(crate) use mcp::mcp_requires_trust;

/// Maximum number of prompt history entries aggregated per directory.
const PROMPT_HISTORY_CAP: usize = 200;

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
    /// The connected servers as the three resource tools see them (US-012).
    /// Those tools are registered once at startup and read this at call time, so
    /// a connect or a disconnect only has to move an entry here.
    pub mcp_resources: agent_mcp::McpResourceCatalog,
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
    /// What the `<environment>` block announces to the model. Same reason as
    /// `sandbox_scope`: the policy is enforced before this loop exists. Holds
    /// the shared permission state, so `/permissions` needs nothing here.
    pub workspace_access: crate::context::WorkspaceAccess,
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

/// The values as the CONFIGURATION resolved them, kept to answer "where does
/// this come from?" (US-005 AC2). `/models`, `/effort` and `/permissions` change
/// them in session, and a layer name would then describe a value that is no
/// longer the one displayed.
struct Resolved {
    model: String,
    reasoning_effort: Option<String>,
    permission_mode: agent_tools::permission::PermissionMode,
}

/// The paths one conversation owns, and the goal-loop counter that travels with
/// them. Grouped because they always move together: four locals reassigned by
/// hand at every switch is how `/fork` ended up missing one of the steps
/// `/resume` performed.
struct Conversation {
    path: PathBuf,
    goal: PathBuf,
    goal_iters: PathBuf,
    /// Automatic re-prompts already spent on the active goal.
    iters: u32,
}

impl Conversation {
    /// Opens the sidecar paths of `path`. `iters` is read back only when a goal
    /// is actually active: a counter without a goal counts nothing.
    fn at(path: PathBuf, goal_active: bool) -> Self {
        let goal = path.with_extension("goal");
        let goal_iters = path.with_extension("goal.iters");
        let iters = if goal_active {
            read_goal_iters(&goal_iters)
        } else {
            0
        };
        Self {
            path,
            goal,
            goal_iters,
            iters,
        }
    }

    fn write_goal(&self, goal: &str) -> std::io::Result<()> {
        std::fs::write(&self.goal, goal)
    }

    fn write_iters(&self) -> std::io::Result<()> {
        std::fs::write(&self.goal_iters, self.iters.to_string())
    }

    /// Forgets the goal of this conversation, sidecars included.
    fn forget_goal(&mut self) -> std::io::Result<()> {
        self.iters = 0;
        remove_if_exists(&self.goal)?;
        remove_if_exists(&self.goal_iters)
    }

    fn session_id(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// How a conversation switch names itself and what it does with the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// `/fork`, `/rewind`: the branch continues the current state, goal included.
    Branch,
    /// `/resume`: the goal is whatever the resumed conversation had.
    Resume,
    /// `/new`, `/clear`: no goal at all, and the sidecars are removed.
    Fresh,
}

impl Switch {
    /// Noun used when the switch itself fails, which is terminal: the previous
    /// runtime is already shut down, so there is nothing to fall back to.
    fn unusable(self) -> &'static str {
        match self {
            Self::Branch => "branch unusable",
            Self::Resume | Self::Fresh => "session unusable",
        }
    }
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

/// Everything the loop mutates while a session is open.
///
/// Held in one place rather than as locals of the event loop: a command that
/// switches conversation has to move six of these at once, and threading them
/// through function arguments is what kept every command inlined in the
/// `select!`.
struct Loop {
    cfg: InteractiveConfig,
    state: AppState,
    runtime: SessionRuntime,
    events: broadcast::Receiver<RuntimeEvent>,
    status: ThreadStatus,
    /// True while the thread runs a turn. Read from the runtime's last-state
    /// signal, never inferred from the events the loop happened to see.
    running: bool,
    conversation: Conversation,
    sessions_dir: PathBuf,
    /// Root of the process cancellation tree. Cloning shares the node, so a
    /// thread opened from here is still a child of the token `run` cancels.
    root: CancellationToken,
    resolved: Resolved,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
    mcp_tool_names: BTreeMap<String, BTreeSet<String>>,
    mcp_tx: mpsc::Sender<McpEvent>,
    /// Answer channel of the permission dialog on screen, if any.
    pending_resp: Option<oneshot::Sender<agent_tools::permission::ApprovalResponse>>,
    /// Start of the current turn (rising edge of `running`) for the elapsed time.
    turn_start: Option<Instant>,
    /// Aggregated workspace diff of the running turn. Opened when a turn starts,
    /// read when it reaches its terminal.
    diff_tracker: Option<agent_tools::turn_diff::TurnDiffTracker>,
    /// US-019: set by `/init`, consumed when the turn it started ends. The project
    /// context is read once, before the turn; only a re-read makes a file written
    /// DURING that turn count for the next one.
    refresh_context: bool,
    /// Set when the loop must stop because the runtime went away (AC8).
    runtime_failure: Option<String>,
    #[cfg(feature = "codex_tui_parity")]
    chat: ChatWidget,
}

/// What one iteration of the loop woke up for.
///
/// The `select!` produces one of these and nothing else: its branches borrow one
/// receiver each and never `&mut Loop`, so the handling below can take the whole
/// state mutably.
enum Wake {
    Terminal(Event),
    /// The keyboard reader is gone: nothing can be typed any more.
    TerminalClosed,
    Runtime(Result<RuntimeEvent, broadcast::error::RecvError>),
    Permission(Option<PermissionMsg>),
    Mcp(Option<McpEvent>),
    HookNotice(Option<String>),
    Spinner,
    CommitTick,
    QuitHintExpired,
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
    cfg: InteractiveConfig,
    sessions_dir: PathBuf,
    current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
    root: &CancellationToken,
) -> anyhow::Result<()> {
    let (mcp_tx, mut mcp_rx) = mpsc::channel::<McpEvent>(16);
    let mut session = Loop::open(cfg, sessions_dir, current_session, mcp, mcp_tx, root).await?;
    session.state.load_history(agent_session::workspace_prompts(
        &session.sessions_dir,
        Some(&session.conversation.path),
        PROMPT_HISTORY_CAP,
    ));

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

    // US-019: a hook running after a tool call reports its failures here. The
    // branch closes for good once the emitter is gone (no hook declared, or
    // registry dropped), so the loop never spins on a closed channel.
    let mut hook_notices_open = true;
    // Spinner animation tick (US-044). 100 ms is about 10 fps: fluid and nearly free
    // (the render cache serves the baked blocks). `Skip` avoids any redraw burst when
    // coming back from idle. The `select!` branch is guarded by `if running` -> 0 CPU when idle.
    let mut spinner = tokio::time::interval(Duration::from_millis(100));
    spinner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut screen = Screen::new();

    loop {
        session.tick_turn_edge();
        screen.draw(tui, &mut session)?;
        if session.state.should_quit {
            break;
        }

        // The guards are read into locals first: a `select!` branch condition
        // that touched `session` again would borrow it while a receiver of the
        // same struct is already borrowed mutably.
        let running = session.running;
        let awaiting_human = session.state.pending.is_some();
        let quit_hint = session.state.quit_shortcut_hint_visible();
        let quit_delay = session
            .state
            .quit_shortcut_remaining()
            .unwrap_or(Duration::ZERO);
        let wake = tokio::select! {
            ev = key_rx.recv() => match ev {
                Some(ev) => Wake::Terminal(ev),
                None => Wake::TerminalClosed,
            },
            received = session.events.recv() => Wake::Runtime(received),
            perm = perm_rx.recv() => Wake::Permission(perm),
            ev = mcp_rx.recv() => Wake::Mcp(ev),
            notice = hook_notices.recv(), if hook_notices_open => Wake::HookNotice(notice),
            // Animation tick: wakes the loop ONLY during an ACTIVE turn and
            // outside a permission wait (we are waiting for the human, not the
            // agent -> 0 CPU, no 10 fps redraw of the dialog). The redraw happens
            // at the head of the loop; this only advances the animation. US-044.
            _ = spinner.tick(), if running && !awaiting_human => Wake::Spinner,
            // Display pacing of the streamed answer (US-019). Separate from the
            // spinner: it releases committed lines to the scrollback at a steady
            // rate whatever the burstiness of the provider, so its cadence must
            // not be tied to the speed of an animation. Owned by `Screen`, which
            // is also what makes it a no-op without the parity frontend.
            _ = screen.pace(), if running => Wake::CommitTick,
            _ = tokio::time::sleep(quit_delay), if quit_hint => Wake::QuitHintExpired,
        };

        match wake {
            // Event channel closed -> we exit.
            Wake::TerminalClosed => break,
            Wake::Terminal(ev) => session.on_terminal_event(ev).await,
            Wake::Runtime(received) => session.on_runtime(received).await,
            Wake::Permission(perm) => session.on_permission(perm),
            Wake::Mcp(ev) => session.on_mcp(ev),
            Wake::HookNotice(Some(message)) => session
                .state
                .blocks
                .push(Block::Notice(format!("Hook: {message}"))),
            Wake::HookNotice(None) => hook_notices_open = false,
            Wake::Spinner => {
                let elapsed = session.turn_start.map(|t| t.elapsed()).unwrap_or_default();
                session.state.tick_progress(elapsed);
            }
            Wake::CommitTick => screen.commit(tui, &mut session)?,
            Wake::QuitHintExpired => session.state.clear_quit_shortcut_hint(),
        }
    }

    // Closes admission, cancels the tree, drains the tasks, writes the terminal
    // the running turn owes and closes the store. No task of this session
    // outlives the loop.
    session.runtime.shutdown().await;
    match session.runtime_failure {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
}

impl Loop {
    async fn open(
        mut cfg: InteractiveConfig,
        sessions_dir: PathBuf,
        current_session: PathBuf,
        mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
        mcp_tx: mpsc::Sender<McpEvent>,
        root: &CancellationToken,
    ) -> anyhow::Result<Self> {
        let resolved = Resolved {
            model: cfg.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone(),
            permission_mode: cfg.permission_mode.get(),
        };
        let mut state = AppState::new(cfg.model.clone(), cfg.truecolor);
        state.set_permission_mode(permission_mode_id(cfg.permission_mode.get()));
        // The workspace the binary resolved, not a second `current_dir()` read:
        // it is the same path the sandbox, the turn diff and `/init` are scoped
        // to, and two sources for one fact is one too many.
        state.workspace = cfg.workspace.to_string_lossy().into_owned();
        state.reasoning_effort = cfg.reasoning_effort.clone();
        state.provider_connected = cfg.connected;
        state.reduced_motion = cfg.reduced_motion;
        state.skills = cfg.skills.names();
        state.files = workspace_file_mentions(&cfg.workspace, 200);
        state.sessions = load_sessions(&sessions_dir, &current_session);
        state.mcp_servers = mcp::metas(&mcp);
        // US-012: only the problems are shown. A silent startup keeps the welcome
        // screen; a server left out is worth losing it.
        for notice in std::mem::take(&mut cfg.mcp_notices) {
            state.blocks.push(Block::Notice(notice));
        }
        // US-016: names handed out per server. Taken out of the config because they
        // change with every connect and disconnect of the session.
        let mcp_tool_names = std::mem::take(&mut cfg.mcp_tool_names);
        let conversation = Conversation::at(current_session, cfg.goal.is_some());

        // The thread runtime of the CURRENT conversation. `/new`, `/resume`, `/fork`
        // and `/rewind` replace it wholesale rather than moving a file under a live
        // writer: a conversation is a thread, and switching conversation is opening
        // another one.
        let OpenSession {
            runtime,
            events,
            status,
        } = open_session(&cfg, &conversation.path, root).await?;
        let initial_messages = runtime.messages();
        if !initial_messages.is_empty() {
            state.blocks = blocks_from_messages(&initial_messages);
            state.blocks.push(Block::Notice(format!(
                "Session resumed ({} messages).",
                initial_messages.len()
            )));
        }
        apply_runtime_status(&mut state, &status);
        // Empty transcript at startup -> the welcome screen (card + logo) shows
        // by itself (see `AppState::is_welcome`), no Notice to push.
        Ok(Self {
            running: is_running(&status),
            state,
            runtime,
            events,
            status,
            conversation,
            sessions_dir,
            root: root.clone(),
            resolved,
            mcp,
            mcp_tool_names,
            mcp_tx,
            pending_resp: None,
            turn_start: None,
            diff_tracker: None,
            refresh_context: false,
            runtime_failure: None,
            #[cfg(feature = "codex_tui_parity")]
            chat: ChatWidget::new(&initial_messages),
            cfg,
        })
    }

    /// Rising/falling edge of `running`: starts / freezes the progress
    /// tracking (spinner, duration, tokens). Does NOT alter the orchestration.
    fn tick_turn_edge(&mut self) {
        match (self.running, self.turn_start.is_some()) {
            (true, false) => {
                self.turn_start = Some(Instant::now());
                self.state.begin_turn();
            }
            (false, true) => {
                self.turn_start = None;
                self.state.end_turn();
            }
            _ => {}
        }
    }

    /// Re-reads the runtime's last state after an operation that may have moved
    /// it, and mirrors it into the frontend.
    fn refresh_status(&mut self) {
        self.status = self.runtime.status();
        self.running = is_running(&self.status);
        apply_runtime_status(&mut self.state, &self.status);
    }

    /// Pushes the session settings the loop owns into the cell the runtime captures
    /// each turn from. Called after every command that moves one of them, so the
    /// NEXT turn is captured from what the user last asked for.
    fn sync_settings(&self) {
        let cfg = &self.cfg;
        cfg.settings.update(|settings| {
            settings.model = cfg.model.clone();
            settings.reasoning_effort = cfg.reasoning_effort.clone();
            settings.goal = cfg.goal.clone();
            settings.permission_mode = permission_mode_id(cfg.permission_mode.get()).to_string();
        });
    }

    /// US-005 AC2: provenance is stated only for the values still as the
    /// configuration resolved them. A layer that no longer explains the displayed
    /// value would be worse than no layer at all: it would be a wrong answer to
    /// "where does this come from?".
    fn config_sources(&self) -> Vec<(&'static str, &'static str)> {
        let mut sources = self.cfg.config_sources.clone();
        let mut drop_key = |key: &str| sources.retain(|(owned, _)| *owned != key);
        if self.cfg.model != self.resolved.model {
            drop_key(agent_tui::SOURCE_KEY_MODEL);
        }
        if self.cfg.reasoning_effort != self.resolved.reasoning_effort {
            drop_key(agent_tui::SOURCE_KEY_REASONING_EFFORT);
        }
        if self.cfg.permission_mode.get() != self.resolved.permission_mode {
            drop_key(agent_tui::SOURCE_KEY_PERMISSION_MODE);
        }
        sources
    }

    /// Quit path. The turn is NOT killed here: the runtime's shutdown closes
    /// admission, cancels the tree, drains the tasks and writes the terminal the
    /// turn owes. All this does is stop waiting on a human and let the loop exit.
    fn begin_shutdown(&mut self) {
        self.answer_pending(agent_tools::permission::ApprovalResponse::DENY_ONCE);
        self.turn_start = None;
        self.state.end_turn();
        self.state.show_shutdown_in_progress();
        self.state.should_quit = true;
    }

    /// Answers the dialog on screen, if one is waiting. A no-op otherwise, which
    /// is what lets every path that ends a wait share one spelling.
    fn answer_pending(&mut self, answer: agent_tools::permission::ApprovalResponse) {
        if let Some(resp) = self.pending_resp.take() {
            let _ = resp.send(answer);
        }
    }

    /// The runtime went away, or a store refused a write it cannot retry.
    /// Nothing can run any more, so the session ends on a NAMED error rather
    /// than on a composer that accepts input nobody will read (AC8).
    fn fail(&mut self, reason: String) {
        self.runtime_failure = Some(reason);
        self.state.should_quit = true;
    }

    // ───────────────────────── frontend adapters ─────────────────────────
    //
    // The parity frontend keeps a transcript of its own, so every local block
    // and every engine event has to reach both. Grouped here so the `#[cfg]`
    // pairs live in four places instead of being repeated at every call site.

    fn route_key(&mut self, key: crossterm::event::KeyEvent) -> InputAction {
        #[cfg(feature = "codex_tui_parity")]
        {
            self.chat.route_key(&mut self.state, key)
        }
        #[cfg(not(feature = "codex_tui_parity"))]
        {
            self.state.on_key(key)
        }
    }

    fn route_paste(&mut self, pasted: &str) {
        #[cfg(feature = "codex_tui_parity")]
        {
            self.chat.route_paste(&mut self.state, pasted);
        }
        #[cfg(not(feature = "codex_tui_parity"))]
        {
            if self.state.pending.is_none() {
                self.state.insert_paste(pasted);
            }
        }
    }

    fn push_user(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.state.push_user(text.clone());
        #[cfg(feature = "codex_tui_parity")]
        self.chat.push_user_message(&self.state, text);
        #[cfg(not(feature = "codex_tui_parity"))]
        let _ = text;
    }

    fn replace_transcript(&mut self, messages: &[Message]) {
        self.state.blocks = blocks_from_messages(messages);
        #[cfg(feature = "codex_tui_parity")]
        self.chat.replace_messages(messages);
    }

    /// Files an engine event into both transcripts. The local blocks are flushed
    /// FIRST, so a notice pushed just before keeps its place in the ordering.
    fn apply_engine_event(&mut self, event: &AgentEvent) {
        #[cfg(feature = "codex_tui_parity")]
        self.chat.sync_local_blocks(&self.state);
        self.state.apply(event);
        #[cfg(feature = "codex_tui_parity")]
        self.chat.handle_agent_event(&self.state, event);
    }

    // ───────────────────────────── input ─────────────────────────────

    async fn on_terminal_event(&mut self, ev: Event) {
        let key = match ev {
            // wheel -> transcript scroll (mouse capture enabled).
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.state.scroll_up(3),
                    MouseEventKind::ScrollDown => self.state.scroll_down(3),
                    _ => {}
                }
                return;
            }
            Event::Paste(pasted) => {
                self.route_paste(&pasted);
                return;
            }
            // normal keystroke; we ignore release repeats.
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            // key release, resize, ... -> plain redraw
            other => {
                if let Event::Resize(w, h) = other {
                    agent_tui::debug_log::log(&format!("event: resize {w}x{h}"));
                }
                return;
            }
        };
        match self.route_key(key) {
            InputAction::Submit(prompt) => self.submit_prompt(prompt).await,
            InputAction::Command(line) => {
                let mut it = line.splitn(2, ' ');
                let cmd = it.next().unwrap_or("").to_string();
                let arg = it.next().unwrap_or("").trim().to_string();
                self.command(&cmd, &arg).await;
                self.state.scroll = 0;
            }
            InputAction::Quit => self.begin_shutdown(),
            InputAction::Interrupt if self.running => {
                self.answer_pending(agent_tools::permission::ApprovalResponse::DENY_ONCE);
                // The runtime signals the turn's own cancellation node and
                // acknowledges at once; the terminal is written after the
                // model, the tools and their process trees have stopped and
                // the transcript was reconciled.
                if let Err(err) = self.runtime.interrupt(None).await {
                    self.state
                        .blocks
                        .push(Block::Error(format!("interrupt: {err}")));
                }
            }
            InputAction::Interrupt => {}
            InputAction::Permission { allow, remember } => {
                // The picker offers allow/deny with an optional session scope;
                // the free-text rejection and the turn-ending abort are
                // reachable from the composer, not from this two-axis choice.
                let answer = if allow {
                    agent_tools::permission::ApprovalResponse::Approved { remember }
                } else {
                    agent_tools::permission::ApprovalResponse::Denied {
                        remember,
                        rejection: None,
                    }
                };
                self.answer_pending(answer);
                #[cfg(feature = "codex_tui_parity")]
                self.chat.record_approval_decision(allow);
            }
            _ => {}
        }
    }

    async fn submit_prompt(&mut self, prompt: String) {
        // US-017 AC2: the submission is gated by its hooks before anything else
        // happens. A refusal keeps the message in the composer and names the
        // reason, so the user can amend it.
        if self
            .cfg
            .hooks
            .watches(agent_tools::HookEvent::UserPromptSubmit)
        {
            let hooks = Arc::clone(&self.cfg.hooks);
            let decision = hooks
                .lifecycle(agent_tools::Lifecycle::UserPromptSubmit { prompt: &prompt })
                .await;
            // US-018: the run is filed like any other, so a lifecycle hook is as
            // visible as a tool one.
            self.state
                .apply(&AgentEvent::Hook(agent_tools::registry::hook_run_view(
                    agent_tools::HookEvent::UserPromptSubmit,
                    None,
                    &decision,
                )));
            if let agent_tools::HookDecision::Deny(reason) = decision {
                self.state
                    .blocks
                    .push(Block::Error(format!("Prompt refused: {reason}")));
                self.state.set_input(prompt);
                return;
            }
        }
        // US-016: `/<skill> …` injects the skill instructions instead of sending
        // its name. Resolved HERE, at submission, so an unreadable skill blocks
        // the turn while the user is looking. The body enters the STEP context,
        // so it reaches the next model request whether this input opened a turn
        // or steered one, and it is still never persisted.
        let injected = match crate::skills::invocation(&self.cfg.skills, &prompt) {
            Some(Ok(injection)) => {
                if injection.truncated {
                    self.state.blocks.push(Block::Notice(format!(
                        "Skill \"{}\" injected, body truncated at the byte budget.",
                        injection.name
                    )));
                }
                let section = format!("skill:{}", injection.name);
                self.cfg.steps.inject(section.clone(), injection.block);
                Some(section)
            }
            Some(Err(err)) => {
                self.state
                    .blocks
                    .push(Block::Error(format!("Skill unusable: {err}")));
                // Nothing is sent, so the typed message goes back to the
                // composer instead of being lost.
                self.state.set_input(prompt);
                return;
            }
            None => None,
        };
        // AC6: `expected_turn_id = None` accepts EITHER branch of the
        // steer/terminal race, which is what a typed message needs: it steers the
        // turn that is running, or opens one of its own if that turn ended
        // meanwhile. Never a post-turn FIFO, and never lost.
        let was_running = self.running;
        match self
            .runtime
            .steer(Submission::new(prompt.clone()), None)
            .await
        {
            Ok(_) => {
                if !was_running {
                    self.conversation.iters = 0;
                }
                self.push_user(prompt);
                if was_running {
                    self.state
                        .blocks
                        .push(Block::Notice("Steering the current turn.".into()));
                }
            }
            Err(err) => {
                // Only what this input added: another injection may belong to
                // the turn that is running.
                if let Some(section) = &injected {
                    self.cfg.steps.remove_injection(section);
                }
                // AC8: a store that refuses is a named error and the session goes
                // on; a runtime that STOPPED is terminal, because nothing can run
                // any more.
                if matches!(err, SubmitError::Stopped) {
                    self.fail(format!("runtime stopped: {err}"));
                }
                self.state
                    .blocks
                    .push(Block::Error(format!("Input refused: {err}")));
                self.state.set_input(prompt);
            }
        }
        self.refresh_status();
    }

    fn on_permission(&mut self, perm: Option<PermissionMsg>) {
        let Some((req, resp)) = perm else {
            return;
        };
        self.state.pending = Some(to_prompt(&req));
        #[cfg(feature = "codex_tui_parity")]
        self.chat
            .handle_permission_request(PermissionTranscriptRequest {
                call_id: req.call_id.clone(),
                tool: req.tool.clone(),
                reason: req.reason.clone(),
                taint_forced: req.taint_forced,
                input_summary: req.input_summary.clone(),
                mode: req.mode.to_string(),
                input: req.input.clone(),
            });
        self.pending_resp = Some(resp);
    }

    // ───────────────────────── conversation switch ─────────────────────────

    /// Closes the current thread and opens the one at `path`.
    ///
    /// Single entry point of `/new`, `/clear`, `/resume`, `/fork` and `/rewind`.
    /// Written once because the steps are not optional: the runtime, its event
    /// stream, its status, the sidecar paths, the transcript, the session list
    /// AND the taint of the messages the conversation resumes on all have to move
    /// together. `/fork` used to skip the last one, which left a branch running
    /// without the forced confirmation `/resume` applied to the very same
    /// transcript.
    ///
    /// Returns the messages of the opened conversation; the caller owns the
    /// notice, because only it knows what it just did.
    async fn switch_to(
        &mut self,
        path: PathBuf,
        switch: Switch,
        label: &str,
    ) -> Option<Vec<Message>> {
        self.runtime.shutdown().await;
        let opened = match open_session(&self.cfg, &path, &self.root).await {
            Ok(opened) => opened,
            Err(err) => {
                self.fail(format!("{label}: {}: {err}", switch.unusable()));
                return None;
            }
        };
        self.runtime = opened.runtime;
        self.events = opened.events;
        self.status = opened.status;
        self.running = is_running(&self.status);

        // The goal follows the switch: a branch continues the current one, a
        // resume adopts the one its sidecar carries, a fresh session has none.
        let goal = match switch {
            Switch::Branch => self.cfg.goal.clone(),
            Switch::Resume => read_goal(&path.with_extension("goal")),
            Switch::Fresh => None,
        };
        let carried_iters = self.conversation.iters;
        self.cfg.goal = goal;
        self.conversation = Conversation::at(path, self.cfg.goal.is_some());
        if switch == Switch::Branch {
            // A branch starts from the current state, goal and counter included:
            // one that reset either would not be the same session.
            self.conversation.iters = carried_iters;
            if let Some(goal) = self.cfg.goal.clone()
                && let Err(err) = self
                    .conversation
                    .write_goal(&goal)
                    .and_then(|()| self.conversation.write_iters())
            {
                self.state.blocks.push(Block::Error(format!("goal: {err}")));
            }
        }
        if switch == Switch::Fresh
            && let Err(err) = self.conversation.forget_goal()
        {
            self.state.blocks.push(Block::Error(format!("goal: {err}")));
        }
        self.sync_settings();

        let messages = self.runtime.messages();
        // The transcript a conversation resumes on may end on untrusted tool
        // output. The taint window is measured in dispatch cycles and has
        // expired by the time a switch is even possible, so it is reseeded from
        // the messages themselves, on EVERY way in.
        self.cfg.engine.tools.seed_taint(recent_untrusted_content(
            &messages,
            crate::RESUME_TAINT_SCAN_MESSAGES,
        ));
        self.replace_transcript(&messages);
        self.state.sessions = load_sessions(&self.sessions_dir, &self.conversation.path);
        apply_runtime_status(&mut self.state, &self.status);
        Some(messages)
    }

    // ───────────────────────── runtime events ─────────────────────────

    async fn on_runtime(&mut self, received: Result<RuntimeEvent, broadcast::error::RecvError>) {
        match received {
            Ok(event) => {
                self.on_runtime_event(event).await;
                self.refresh_status();
                // AC8: the actor closed its admission without being asked to.
                if self.status.shutting_down && !self.state.should_quit {
                    self.fail(
                        "the thread runtime closed its admission: no further turn can run".into(),
                    );
                }
            }
            // The durable state is the store, so a dropped LIVE event costs
            // the display a line, not the session (edge case #18).
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                self.state.blocks.push(Block::Notice(format!(
                    "{dropped} runtime event(s) dropped from the live stream; the \
                     transcript on disk stays complete."
                )));
                self.status = self.runtime.status();
                if let agent_runtime::ThreadHealth::StoreFailed { operation, detail } =
                    &self.status.health
                {
                    let failure = format!("thread store failed during {operation}: {detail}");
                    self.state.blocks.push(Block::Error(failure.clone()));
                    self.fail(failure);
                }
                self.running = is_running(&self.status);
                apply_runtime_status(&mut self.state, &self.status);
            }
            // AC8: the runtime went away. The terminal is restored by `run`,
            // and the session ends on a named error instead of a panic or a
            // silent freeze.
            Err(broadcast::error::RecvError::Closed) => {
                self.fail("the thread runtime stopped: no further turn can run".into());
            }
        }
    }

    async fn on_runtime_event(&mut self, event: RuntimeEvent) {
        match event.payload {
            // US-005 AC2: the engine event is forwarded with its canonical
            // content untouched; only its correlation is added, and the frontend
            // renders what it always did.
            RuntimeEventPayload::Engine(ev) => {
                // Calibration probe (US-002): in interactive mode the TUI owns
                // the terminal, so the line goes to the debug log, never to a
                // process output.
                if let AgentEvent::ModelTurn(view) = &ev
                    && let Some(line) = crate::jsonl::usage_probe_line(view)
                {
                    agent_tui::debug_log::log(&line);
                }
                self.apply_engine_event(&ev);
            }
            RuntimeEventPayload::TurnStateChanged { to, ref cause, .. } => {
                if to == TurnState::Running {
                    // Diff reference taken when the turn really starts, hence
                    // before its first tool write.
                    self.diff_tracker = Some(
                        agent_tools::turn_diff::TurnDiffTracker::begin(&self.cfg.workspace).await,
                    );
                }
                if to.is_terminal() {
                    self.on_terminal_turn(to, cause.as_deref(), event.thread_id, event.turn_id)
                        .await;
                }
            }
            // The input is already in the transcript: it was displayed when it
            // was accepted.
            RuntimeEventPayload::InputAccepted { .. }
            | RuntimeEventPayload::Forked { .. }
            | RuntimeEventPayload::ShuttingDown => {}
            RuntimeEventPayload::StoreFailed { operation, detail } => {
                let failure = format!("thread store failed during {operation}: {detail}");
                self.state.blocks.push(Block::Error(failure.clone()));
                self.fail(failure);
            }
        }
    }

    async fn on_terminal_turn(
        &mut self,
        to: TurnState,
        cause: Option<&str>,
        thread_id: agent_runtime::ThreadId,
        turn_id: Option<agent_runtime::TurnId>,
    ) {
        // Aggregated after the last tool write, including when the turn was
        // interrupted.
        if let Some(mut tracker) = self.diff_tracker.take() {
            match tracker.turn_diff().await {
                Ok(diff) if !diff.is_empty() => {
                    self.apply_engine_event(&AgentEvent::TurnDiff(diff));
                }
                Ok(_) => {}
                Err(err) => agent_tui::debug_log::log(&format!("turn diff: {err}")),
            }
        }
        // US-019 AC1: a terminal cause reaches the transcript with the SAME
        // category, the same next step and the same identifiers the other three
        // surfaces show. Dropping it here is what used to make a failed turn look
        // like no reaction at all.
        if let Some(failure) = agent_runtime::TurnFailure::classify(to, cause) {
            let line = crate::failure_line::render(&failure, thread_id, turn_id);
            // A cancellation or a shutdown is not a fault: it says so in words
            // either way, but tinting it red would report a failure the user
            // caused on purpose.
            self.state.blocks.push(match failure.category {
                agent_runtime::FailureCategory::Interrupted => Block::Notice(line),
                _ => Block::Error(line),
            });
        }
        // US-019 AC1 of the harness PRD: re-read BEFORE any continuation turn, so
        // the goal loop sees the fresh context too.
        if std::mem::take(&mut self.refresh_context) {
            self.cfg.steps.set_project(crate::context::project_messages(
                &self.cfg.workspace,
                &crate::context::today_utc(),
                &self.cfg.skills,
                &self.cfg.workspace_access,
            ));
            self.state.blocks.push(Block::Notice(
                match crate::context::instructions_file(&self.cfg.workspace) {
                    Some(name) => format!(
                        "{name} is part of the project context from the next model request on."
                    ),
                    None => {
                        "No instruction file written: the project context is unchanged.".to_string()
                    }
                },
            ));
        }
        // The runtime may already have started the next queued input: what it
        // says is what counts.
        self.status = self.runtime.status();
        self.running = is_running(&self.status);
        // A body injected for a turn survives an input that opens the next one
        // immediately, and is dropped when the thread goes quiet.
        if !self.running {
            self.cfg.steps.clear_injections();
        }
        if to == TurnState::Completed && self.cfg.goal.is_some() && !self.running {
            self.advance_goal_loop().await;
            self.status = self.runtime.status();
            self.running = is_running(&self.status);
        }
        // `Stop` fires when the agent really stops, hence not when the goal loop
        // or a queued input opens another turn right away.
        if !self.running && self.cfg.hooks.watches(agent_tools::HookEvent::Stop) {
            let hooks = Arc::clone(&self.cfg.hooks);
            let decision = hooks.lifecycle(agent_tools::Lifecycle::Stop).await;
            self.state
                .apply(&AgentEvent::Hook(agent_tools::registry::hook_run_view(
                    agent_tools::HookEvent::Stop,
                    None,
                    &decision,
                )));
        }
    }

    /// Goal loop: on a CLEAN end of turn with an active goal and nothing else
    /// queued, re-prompt as long as the completion marker is not emitted.
    async fn advance_goal_loop(&mut self) {
        if take_goal_done(&mut self.state) {
            self.cfg.goal = None;
            self.sync_settings();
            if let Err(err) = self.conversation.forget_goal() {
                self.state.blocks.push(Block::Error(format!("goal: {err}")));
            }
            self.state
                .blocks
                .push(Block::Notice("Goal completed and cleared.".into()));
            return;
        }
        if self.conversation.iters >= MAX_GOAL_ITERS {
            self.state.blocks.push(Block::Notice(format!(
                "Goal not confirmed after {MAX_GOAL_ITERS} retries. Use /goal clear to abandon it."
            )));
            return;
        }
        self.conversation.iters += 1;
        if let Err(err) = self.conversation.write_iters() {
            self.state.blocks.push(Block::Error(format!("goal: {err}")));
            return;
        }
        let iters = self.conversation.iters;
        self.state.blocks.push(Block::Notice(format!(
            "Continuing goal ({iters}/{MAX_GOAL_ITERS})..."
        )));
        self.turn_start = None;
        self.state.end_turn();
        // A continuation is an INPUT like any other: durable before it is
        // acknowledged (FR-05), so the log says why the next turn exists.
        if let Err(err) = self
            .runtime
            .submit(Submission::new(GOAL_CONTINUE_PROMPT))
            .await
        {
            self.state
                .blocks
                .push(Block::Error(format!("goal: continuation refused: {err}")));
        }
    }
}

/// Terminal-side state of the loop: what has been written to the scrollback and
/// at which width. Separate from [`Loop`] because none of it is a fact about the
/// conversation, and because it is the only part the parity feature replaces
/// wholesale.
#[cfg(feature = "codex_tui_parity")]
struct Screen {
    inserter: HistoryInserter,
    /// Terminal width the scrollback was last written at, and the deadline of a
    /// pending rewrite.
    reflow_width: Option<u16>,
    reflow_due: Option<Instant>,
    last_geometry: Option<String>,
    commit_tick: tokio::time::Interval,
}

#[cfg(feature = "codex_tui_parity")]
impl Screen {
    fn new() -> Self {
        let mut commit_tick = tokio::time::interval(agent_tui::COMMIT_TICK_INTERVAL);
        commit_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            inserter: HistoryInserter::new(InsertHistoryMode::InlineScrollback),
            reflow_width: None,
            reflow_due: None,
            last_geometry: None,
            commit_tick,
        }
    }

    /// Resolves when the streamed answer should release its committed lines.
    /// Cancel-safe: the cadence lives in the interval, not in this future.
    async fn pace(&mut self) {
        self.commit_tick.tick().await;
    }

    fn commit(&mut self, tui: &mut agent_tui::Tui, session: &mut Loop) -> anyhow::Result<()> {
        let width = tui.size()?.width;
        session
            .chat
            .surface_mut()
            .commit_tick(width, Instant::now());
        Ok(())
    }

    fn draw(&mut self, tui: &mut agent_tui::Tui, session: &mut Loop) -> anyhow::Result<()> {
        let size = tui.size()?;
        session.chat.sync_local_blocks(&session.state);
        // A width change invalidates every row already written: the terminal
        // does not rewrap what it was handed. The transcript cells are the
        // source of truth, so the scrollback is dropped and rewritten. The
        // debounce covers drag-resize, which emits one event per column.
        match self.reflow_width {
            Some(previous) if previous != size.width => {
                self.reflow_due = Some(Instant::now() + agent_tui::REFLOW_DEBOUNCE);
                self.reflow_width = Some(size.width);
            }
            None => self.reflow_width = Some(size.width),
            _ => {}
        }
        if self.reflow_due.is_some_and(|due| Instant::now() >= due) {
            self.reflow_due = None;
            if self.inserter.mode() == InsertHistoryMode::InlineScrollback {
                // Only the rows still on screen can be rewritten, so only the
                // cells that fit back into them are replayed. Replaying more
                // would scroll the repaired transcript straight past the old
                // one, showing both.
                let rows = agent_tui::clear_for_reflow(tui)?;
                session.chat.surface_mut().reflow(size.width, rows as usize);
            }
        }
        if agent_tui::debug_log::enabled() {
            let viewport = tui.viewport_area;
            let line = format!(
                "frame: screen={}x{} viewport=(x{} y{} w{} h{})",
                size.width, size.height, viewport.x, viewport.y, viewport.width, viewport.height
            );
            if self.last_geometry.as_deref() != Some(line.as_str()) {
                agent_tui::debug_log::log(&line);
                self.last_geometry = Some(line);
            }
        }
        // Finalized cells go to the terminal scrollback ONCE, above the
        // viewport. The renderer below never draws them again: the terminal
        // owns them, which is what makes its own scroll and selection work
        // on the transcript.
        if self.inserter.mode() == InsertHistoryMode::InlineScrollback
            && let Some(insert) = session
                .chat
                .surface_mut()
                .drain_pending_insert(size.width, self.inserter.mode())
            && let Err(err) = self.inserter.insert(tui, &insert)
        {
            session
                .state
                .blocks
                .push(Block::Notice(err.message().to_string()));
        }
        let height = agent_tui::parity_content_height(
            &session.state,
            session.chat.surface(),
            size.width,
            size.height,
        );
        let (chat, state) = (&session.chat, &session.state);
        if self.inserter.mode() == InsertHistoryMode::InlineScrollback {
            agent_tui::draw(tui, height, |frame| chat.render(frame, state))?;
        } else {
            agent_tui::draw(tui, size.height, |frame| agent_tui::render(frame, state))?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "codex_tui_parity"))]
struct Screen;

#[cfg(not(feature = "codex_tui_parity"))]
impl Screen {
    fn new() -> Self {
        Self
    }

    /// No streaming surface to pace: the branch is kept so the `select!` stays
    /// one expression, and it simply never fires.
    async fn pace(&mut self) {
        std::future::pending::<()>().await
    }

    fn commit(&mut self, _tui: &mut agent_tui::Tui, _session: &mut Loop) -> anyhow::Result<()> {
        Ok(())
    }

    fn draw(&mut self, tui: &mut agent_tui::Tui, session: &mut Loop) -> anyhow::Result<()> {
        let size = tui.size()?;
        let state = &session.state;
        agent_tui::draw(tui, size.height, |frame| agent_tui::render(frame, state))?;
        Ok(())
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

pub(crate) fn read_goal(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_goal_iters(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
        Conversation, GOAL_DONE_MARKER, Switch, apply_runtime_status, compose_system, is_running,
        session_path_from_arg, take_goal_done, workspace_file_mentions,
    };
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
        let source = concat!(
            include_str!("mod.rs"),
            include_str!("commands.rs"),
            include_str!("mcp.rs")
        );
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

    /// The taint of the transcript a conversation resumes on is reseeded on
    /// EVERY way in.
    ///
    /// `/fork` and `/rewind` used to open their branch with a copy of the
    /// `/resume` body that had dropped the reseed. The taint window is measured
    /// in dispatch cycles and has expired by the time a branch is even possible,
    /// so a branch of a conversation ending on untrusted tool output ran without
    /// the forced confirmation `/resume` applied to those very same messages.
    ///
    /// The guarantee is structural now: `open_session` is reached from exactly
    /// two places, the first conversation and `switch_to`, and `switch_to` is
    /// the only thing that seeds. A third caller would be a third policy.
    #[test]
    fn every_conversation_switch_reseeds_the_taint_of_its_transcript() {
        let source = concat!(
            include_str!("mod.rs"),
            include_str!("commands.rs"),
            include_str!("mcp.rs")
        );
        // Split so this assertion does not match itself.
        assert_eq!(
            source.matches(concat!("open_session", "(&")).count(),
            2,
            "a third way into a conversation is a third chance to forget a step \
             `switch_to` performs"
        );
        assert_eq!(
            source.matches(concat!("seed_taint", "(")).count(),
            1,
            "the reseed belongs to the single switch, not to each command"
        );
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

    /// The sidecars of a conversation are derived from ONE path, so a switch
    /// cannot leave the goal of the previous conversation pointing at the file
    /// of the new one.
    #[test]
    fn a_conversation_derives_its_sidecars_from_its_log() {
        let dir = std::env::temp_dir().join(format!("pyxis-conv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("123.jsonl");

        let conversation = Conversation::at(path.clone(), false);
        assert_eq!(conversation.goal, dir.join("123.goal"));
        assert_eq!(conversation.goal_iters, dir.join("123.goal.iters"));
        assert_eq!(conversation.session_id(), "123.jsonl");
        assert_eq!(conversation.iters, 0);

        // The counter is only read back when a goal is actually active.
        conversation.write_goal("finir le refactor").unwrap();
        std::fs::write(&conversation.goal_iters, "7").unwrap();
        assert_eq!(Conversation::at(path.clone(), false).iters, 0);
        let mut resumed = Conversation::at(path, true);
        assert_eq!(resumed.iters, 7);

        resumed.forget_goal().unwrap();
        assert!(!resumed.goal.exists());
        assert!(!resumed.goal_iters.exists());
        assert_eq!(resumed.iters, 0);
        // Forgetting twice is not an error: the files are simply gone.
        resumed.forget_goal().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every switch names itself, and only a branch keeps the goal of the
    /// conversation it came from.
    #[test]
    fn a_switch_names_its_own_failure() {
        assert_eq!(Switch::Branch.unusable(), "branch unusable");
        assert_eq!(Switch::Resume.unusable(), "session unusable");
        assert_eq!(Switch::Fresh.unusable(), "session unusable");
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
}
