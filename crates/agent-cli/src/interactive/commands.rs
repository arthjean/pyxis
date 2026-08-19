//! The slash commands of the interactive loop.
//!
//! One method per command, on the loop's own state. They used to be arms of a
//! single 600-line `match` inlined in the `select!`, which is what made the
//! guards ("wait for the turn to finish") and the conversation switch appear
//! four times each.
//!
//! Two rules hold the module together:
//! - a command NEVER touches the turn lifecycle. It submits, steers, forks or
//!   interrupts through the runtime and reads the state back (`refresh_status`);
//! - a command that needs a terminal turn boundary declares it in
//!   [`NEEDS_IDLE`] instead of opening with a guard of its own.

use std::path::PathBuf;
use std::time::Duration;

use agent_core::Session;
use agent_core::message::{ContentBlock, Message};
use agent_runtime::thread::Submission;
use agent_tui::{
    Block, COMMANDS, default_reasoning_effort_for_model, normalize_reasoning_effort_for_model,
    supported_reasoning_efforts_for_model,
};

use super::{Loop, Switch, new_session_path, sign_out};
use crate::settings::{permission_mode_from_arg, permission_mode_id};

/// Commands the dispatcher below answers. Checked against `agent_tui::COMMANDS`
/// by a test: the picker lives in the frontend crate and the handling here, so
/// a command offered by one and unknown to the other would silently fall
/// through to "Unknown command".
///
/// Deliberately not a runtime lookup (the `match` below is the dispatch): it is
/// the list a reader updates next to the arm they just added, and the test is
/// what makes forgetting it fail.
#[cfg_attr(not(test), allow(dead_code))]
const HANDLED_COMMANDS: &[&str] = &[
    "/help",
    "/models",
    "/effort",
    "/permissions",
    "/skills",
    "/goal",
    "/providers",
    "/mcp",
    "/resume",
    "/fork",
    "/rewind",
    "/approvals",
    "/status",
    "/usage",
    "/hooks",
    "/diff",
    "/copy",
    "/init",
    "/compact",
    "/new",
    "/clear",
    "/logout",
    "/quit",
];

/// Commands that need a TERMINAL turn boundary. A branch is cut at one, a
/// compaction rewrites the transcript, and `/goal` and `/init` open a turn of
/// their own: all four would otherwise work on a conversation that is still
/// moving (edge case #12).
const NEEDS_IDLE: &[&str] = &[
    "/goal", "/resume", "/new", "/clear", "/fork", "/rewind", "/init", "/compact",
];

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

/// What `/logout` can promise and what it cannot (US-019 AC5). Nothing here
/// reaches OpenAI: the credential is deleted locally, so the ChatGPT session
/// itself stays open until the user revokes it from their account.
const LOGOUT_SERVER_NOTE: &str = "The ChatGPT session is NOT revoked server-side: \
     only the local credential is deleted. Revoke it from your OpenAI account to \
     close it everywhere.";

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

/// Ceiling on a clipboard helper. They all fork into the background, so the
/// foreground process returns at once; a helper that does not is a hang of the
/// WHOLE interface, since this runs inside the event loop.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(3);

impl Loop {
    /// Routes one `/command [arg]`.
    pub(super) async fn command(&mut self, cmd: &str, arg: &str) {
        if self.running && NEEDS_IDLE.contains(&cmd) {
            self.state.blocks.push(Block::Notice(
                if cmd == "/compact" {
                    "A turn is in progress."
                } else {
                    "Wait for the current turn to finish."
                }
                .into(),
            ));
            return;
        }
        match cmd {
            "/help" => {
                let list = COMMANDS
                    .iter()
                    .map(|(n, _, _)| *n)
                    .collect::<Vec<_>>()
                    .join("  ");
                self.state
                    .blocks
                    .push(Block::Notice(format!("Commands: {list}")));
            }
            "/models" => self.cmd_models(arg).await,
            "/effort" => self.cmd_effort(arg),
            "/permissions" => self.cmd_permissions(arg),
            "/goal" => self.cmd_goal(arg).await,
            // US-017 AC2/AC3/AC4: the branch is asked of the RUNTIME, at a named
            // terminal boundary or at the last one. The source thread is neither
            // truncated, rewritten nor deleted: it stays on disk exactly as it
            // was, and the client switches to the branch.
            "/fork" | "/rewind" => self.cmd_branch(cmd, arg).await,
            "/resume" | "/new" | "/clear" => self.cmd_open(cmd, arg).await,
            // US-005: purely local surfaces (no network call), pushed as notices
            // so they never enter the transcript.
            "/status" => self.cmd_status(),
            "/usage" => {
                let report = agent_tui::session_usage_report(&self.state);
                self.state.blocks.push(Block::Notice(report));
            }
            // US-009: inspection surface of the session memory. Local only, and
            // the memory never leaves the process.
            "/approvals" => {
                let report = match arg {
                    "clear" => {
                        let n = self.cfg.approvals.clear();
                        format!("{n} remembered answer(s) forgotten.")
                    }
                    "" => approvals_report(&self.cfg.approvals.entries()),
                    other => {
                        format!("Unknown argument: {other}. Usage: /approvals [clear]")
                    }
                };
                self.state.blocks.push(Block::Notice(report));
            }
            "/diff" => self.cmd_diff().await,
            "/copy" => self.cmd_copy().await,
            // US-019: hooks are declared in the GLOBAL settings only, so this
            // list is also the answer to "can this repository run something
            // behind my back?".
            "/hooks" => {
                let report = hooks_report(&self.cfg.hook_specs);
                self.state.blocks.push(Block::Notice(report));
            }
            "/logout" => self.cmd_logout().await,
            "/init" => self.cmd_init(arg).await,
            "/compact" => self.cmd_compact().await,
            "/providers" => self.cmd_providers(arg).await,
            "/mcp" => self.mcp_command(arg),
            "/skills" => self.state.blocks.push(Block::Notice(
                "Choose a skill in the /skills submenu.".into(),
            )),
            "/quit" => self.begin_shutdown(),
            other => self
                .state
                .blocks
                .push(Block::Notice(format!("Unknown command: {other}"))),
        }
    }

    // ───────────────────────── session settings ─────────────────────────

    async fn cmd_models(&mut self, arg: &str) {
        if arg.is_empty() {
            self.state.blocks.push(Block::Notice(
                "Usage : /models <slug> (ex: /models gpt-5.5)".into(),
            ));
            return;
        }
        // The slug is checked against the catalog BEFORE anything is changed or
        // written. An unknown one only fails at the next turn, inside the turn
        // context capture, and it has reached the global settings file by then:
        // the session is then stuck on a model no start can resolve.
        let Some(meta) = agent_tui::model_meta(arg) else {
            let known = agent_tui::models()
                .iter()
                .map(|meta| meta.slug)
                .collect::<Vec<_>>()
                .join(", ");
            self.state.blocks.push(Block::Error(format!(
                "Unknown model: {arg}. Available: {known}"
            )));
            return;
        };
        if let Some(reason) = meta.incompatibility_reason {
            self.state.blocks.push(Block::Error(format!(
                "Model {arg} unusable in this build: {reason}"
            )));
            return;
        }

        let removed = count_encrypted_reasoning(&self.runtime.messages());
        if removed > 0 {
            if let Err(err) = self.runtime.session().redact_encrypted_reasoning().await {
                self.state
                    .blocks
                    .push(Block::Error(format!("models: redaction reasoning: {err}")));
                return;
            }
            let _ = self
                .runtime
                .conversation()
                .lock()
                .map(|mut msgs| scrub_encrypted_reasoning(&mut msgs[..]));
        }
        let next_effort = self
            .cfg
            .reasoning_effort
            .as_deref()
            .and_then(|effort| normalize_reasoning_effort_for_model(arg, effort))
            .or_else(|| default_reasoning_effort_for_model(arg).map(str::to_string));
        self.cfg.model = arg.to_string();
        self.cfg.reasoning_effort = next_effort.clone();
        self.state.model = arg.to_string();
        self.state.reasoning_effort = next_effort.clone();

        // The switch itself says nothing: the status line carries the model and
        // its effort permanently, so a line in the thread would only repeat what
        // the screen already shows, and would keep repeating it for the rest of
        // the session. A redaction is not a setting though: it REMOVED content
        // from the transcript, and nothing else would ever say so.
        if removed > 0 {
            self.state.blocks.push(Block::Notice(format!(
                "{removed} encrypted reasoning item(s) dropped from the transcript: \
                 {arg} cannot resume the reasoning of another model."
            )));
        }
        self.persist_model(arg);
        self.persist_reasoning_effort(next_effort.as_deref());
        self.sync_settings();
    }

    fn cmd_effort(&mut self, arg: &str) {
        let supported = supported_reasoning_efforts_for_model(&self.cfg.model);
        if arg.is_empty() {
            self.state
                .blocks
                .push(Block::Notice(if supported.is_empty() {
                    format!("No known reasoning efforts for model {}", self.cfg.model)
                } else {
                    format!("Usage : /effort <{}>", supported.join("|"))
                }));
            return;
        }
        let Some(effort) = normalize_reasoning_effort_for_model(&self.cfg.model, arg) else {
            self.state
                .blocks
                .push(Block::Notice(if supported.is_empty() {
                    format!("No known reasoning efforts for model {}", self.cfg.model)
                } else {
                    format!(
                        "Unsupported reasoning effort for {}: {arg}. Available: {}",
                        self.cfg.model,
                        supported.join("|")
                    )
                }));
            return;
        };
        self.cfg.reasoning_effort = Some(effort.clone());
        self.state.reasoning_effort = Some(effort.clone());
        // Silent like the model switch: the status line already shows the effort
        // next to the model it belongs to.
        self.persist_reasoning_effort(Some(&effort));
        self.sync_settings();
    }

    fn cmd_permissions(&mut self, arg: &str) {
        if arg.is_empty() {
            self.state.blocks.push(Block::Notice(
                "Usage : /permissions <ask|accept-edits|auto|full-access|read-only>".into(),
            ));
            return;
        }
        let Some(mode) = permission_mode_from_arg(arg) else {
            self.state
                .blocks
                .push(Block::Notice(format!("Unknown permission mode: {arg}")));
            return;
        };
        self.cfg.permission_mode.set(mode);
        let id = permission_mode_id(mode);
        self.state.set_permission_mode(id);
        // Silent as well, and this is the one place Pyxis diverges from Codex,
        // which still writes `Permissions updated to ...` into its thread: the
        // footer indicator names the mode as long as it is not the default one,
        // and its disappearance is what says the default is back.
        if let Some(path) = &self.cfg.settings_path
            && let Err(err) = crate::settings::save_permission_mode(path, mode)
        {
            self.state.blocks.push(Block::Error(format!(
                "settings: failed to save permission mode: {err}"
            )));
        }
        self.sync_settings();
    }

    fn persist_model(&mut self, model: &str) {
        if let Some(path) = &self.cfg.settings_path
            && let Err(err) = crate::settings::save_model(path, model)
        {
            self.state.blocks.push(Block::Error(format!(
                "settings: failed to save model: {err}"
            )));
        }
    }

    fn persist_reasoning_effort(&mut self, effort: Option<&str>) {
        if let Some(path) = &self.cfg.settings_path
            && let Err(err) = crate::settings::save_reasoning_effort(path, effort)
        {
            self.state.blocks.push(Block::Error(format!(
                "settings: failed to save reasoning effort: {err}"
            )));
        }
    }

    // ───────────────────────────── the goal ─────────────────────────────

    async fn cmd_goal(&mut self, arg: &str) {
        match arg {
            "" => {
                let line = match &self.cfg.goal {
                    Some(goal) => format!("Active goal: {goal}"),
                    None => "No goal. Usage: /goal <goal to complete>".into(),
                };
                self.state.blocks.push(Block::Notice(line));
            }
            "clear" => {
                self.cfg.goal = None;
                self.sync_settings();
                if let Err(err) = self.conversation.forget_goal() {
                    self.state.blocks.push(Block::Error(format!("goal: {err}")));
                }
                self.state
                    .blocks
                    .push(Block::Notice("Goal cleared.".into()));
            }
            goal => {
                // Sets the goal of this session and starts the work.
                self.cfg.goal = Some(goal.to_string());
                self.sync_settings();
                self.conversation.iters = 0;
                if let Err(err) = self
                    .conversation
                    .write_goal(goal)
                    .and_then(|()| self.conversation.write_iters())
                {
                    self.state.blocks.push(Block::Error(format!("goal: {err}")));
                }
                match self.runtime.submit(Submission::new(goal)).await {
                    Ok(_) => self.push_user(goal),
                    Err(err) => self
                        .state
                        .blocks
                        .push(Block::Error(format!("goal refused: {err}"))),
                }
                self.refresh_status();
            }
        }
    }

    // ─────────────────────── switching conversation ───────────────────────

    async fn cmd_branch(&mut self, cmd: &str, arg: &str) {
        let at = match parse_turn_argument(arg) {
            Ok(at) => at,
            Err(err) => {
                self.state.blocks.push(Block::Error(err));
                return;
            }
        };
        if cmd == "/rewind" && at.is_none() {
            self.state.blocks.push(Block::Notice(
                "Usage: /rewind <turn-id> (see /status for the current turn)".into(),
            ));
            return;
        }
        let branch = match self.runtime.fork(at).await {
            Ok(branch) => branch,
            Err(err) => {
                self.state
                    .blocks
                    .push(Block::Error(format!("{cmd}: {err}")));
                return;
            }
        };
        let Some(path) = branch.path.clone() else {
            self.state.blocks.push(Block::Error(format!(
                "{cmd}: this session persists nothing to branch from"
            )));
            return;
        };
        let Some(messages) = self.switch_to(path, Switch::Branch, cmd).await else {
            return;
        };
        self.state.blocks.push(Block::Notice(format!(
            "Branch {} created at turn {} ({} messages). The source thread stays \
             on disk untouched.",
            branch.thread_id,
            branch.fork_turn_id,
            messages.len()
        )));
    }

    async fn cmd_open(&mut self, cmd: &str, arg: &str) {
        let (path, switch): (PathBuf, Switch) = if cmd == "/resume" {
            match crate::resolve_resume_path(&self.sessions_dir, arg) {
                Ok(path) => (path, Switch::Resume),
                Err(err) => {
                    self.state.blocks.push(Block::Error(format!("{err}")));
                    return;
                }
            }
        } else {
            (new_session_path(&self.sessions_dir), Switch::Fresh)
        };
        let Some(messages) = self.switch_to(path, switch, cmd).await else {
            return;
        };
        // `/new` opens a new session below the current transcript; `/clear` also
        // wipes the terminal it was written to, so what is left is what a fresh
        // start shows. Asked only once the switch succeeded: a session that
        // could not be replaced still owns what is on screen, and an error is
        // worth nothing on a screen that just lost its context.
        if cmd == "/clear" {
            self.pending_terminal_clear = true;
        }
        // A cleared transcript brings the welcome screen back, which is its own
        // confirmation.
        if !messages.is_empty() {
            self.state.blocks.push(Block::Notice(format!(
                "Session resumed ({} messages).",
                messages.len()
            )));
        } else if cmd == "/resume" {
            self.state
                .blocks
                .push(Block::Notice("Empty session.".into()));
        }
    }

    // ──────────────────────── local inspection ────────────────────────

    fn cmd_status(&mut self) {
        let session_id = self.conversation.session_id();
        let sources = self.config_sources();
        let report = agent_tui::session_status_report(
            &self.state,
            agent_tui::SessionFacts {
                session_id: &session_id,
                sandbox: &self.cfg.sandbox_scope,
                config_sources: &sources,
                profile: self.cfg.profile.as_deref(),
                // What the pipeline did, which the transcript does not carry:
                // counts, failures and average durations per tool, refusals
                // included.
                tool_activity: &self.cfg.registry.dispatch_log().summary(),
                runtime: runtime_facts(),
            },
        );
        self.state.blocks.push(Block::Notice(report));
    }

    /// US-006: the diff reuses the engine of the aggregated turn diff, hence
    /// exactly its scope.
    async fn cmd_diff(&mut self) {
        let block = match agent_tools::turn_diff::workspace_diff(&self.cfg.workspace).await {
            Ok(agent_tools::turn_diff::WorkspaceDiff::NoRepository) => {
                Block::Notice("Diff unavailable: this directory is not a git repository.".into())
            }
            Ok(agent_tools::turn_diff::WorkspaceDiff::Changes(diff)) => {
                Block::Notice(workspace_diff_report(&diff))
            }
            Err(err) => Block::Error(format!("diff: {err}")),
        };
        self.state.blocks.push(block);
    }

    /// US-019: the last answer as it was streamed, no rendering applied. A
    /// clipboard that refuses is an error block, never a silent "copied".
    async fn cmd_copy(&mut self) {
        let block = match last_assistant_text(&self.state) {
            None => Block::Notice("No answer to copy yet.".into()),
            Some(text) if text.is_empty() => Block::Notice("The last answer is empty.".into()),
            Some(text) => match copy_to_clipboard(&text).await {
                Ok(helper) => Block::Notice(format!(
                    "Last answer copied with {helper} ({} characters).",
                    text.chars().count()
                )),
                Err(err) => Block::Error(format!("copy: {err}")),
            },
        };
        self.state.blocks.push(block);
    }

    // ───────────────────────── account and context ─────────────────────────

    async fn cmd_logout(&mut self) {
        if !self.state.provider_connected {
            self.state
                .blocks
                .push(Block::Notice("Already signed out.".into()));
            return;
        }
        match sign_out(&self.cfg.provider).await {
            Ok(()) => {
                self.state.provider_connected = false;
                self.state.blocks.push(Block::Notice(format!(
                    "Signed out: local credential deleted. {LOGOUT_SERVER_NOTE}"
                )));
            }
            Err(err) => self
                .state
                .blocks
                .push(Block::Error(format!("logout: {err}"))),
        }
    }

    async fn cmd_init(&mut self, arg: &str) {
        let decision = init_decision(&self.cfg.workspace, arg);
        if let InitDecision::Confirm(name) = decision {
            self.state.blocks.push(Block::Notice(format!(
                "{name} already exists at the workspace root. Run `/init force` to \
                 have it rewritten."
            )));
            return;
        }
        if let InitDecision::Overwrite(name) = decision {
            self.state.blocks.push(Block::Notice(format!(
                "Rewriting {name} (confirmed by `force`)."
            )));
        }
        // The transcript keeps `/init`; the instructions travel as a step
        // section, exactly like a skill body, so they are never persisted.
        self.cfg.steps.inject("init", INIT_PROMPT.to_string());
        match self.runtime.submit(Submission::new("/init")).await {
            Ok(_) => {
                self.push_user("/init");
                // AC1: the project context was read before this turn wrote the
                // file, so it is re-read when the turn ends, no restart involved.
                self.refresh_context = true;
            }
            Err(err) => {
                self.cfg.steps.clear_injections();
                self.state
                    .blocks
                    .push(Block::Error(format!("init refused: {err}")));
            }
        }
        self.refresh_status();
    }

    async fn cmd_compact(&mut self) {
        let mut messages = self.runtime.messages();
        let before = messages.len();
        let max_output_tokens = self.cfg.settings.read(|s| s.run_config.max_output_tokens);
        if let Err(err) = agent_core::compaction::full_compact(
            &mut messages,
            &self.cfg.model,
            self.cfg.provider.as_ref(),
            max_output_tokens,
        )
        .await
        {
            // `full_compact` leaves the transcript intact on failure: the
            // session stays usable as is.
            self.state
                .blocks
                .push(Block::Error(format!("compact: {err}")));
            return;
        }
        // Persisted like an automatic compaction: same checkpoint entry, hence
        // replayable by `/resume` without the session knowing it was manual.
        if let Err(err) = self
            .runtime
            .session()
            .checkpoint(agent_core::CompactKind::Auto, &messages)
            .await
        {
            self.state
                .blocks
                .push(Block::Error(format!("compact: {err}")));
            return;
        }
        let after = messages.len();
        if let Ok(mut held) = self.runtime.conversation().lock() {
            *held = messages;
        }
        self.state.blocks.push(Block::Notice(format!(
            "Context compacted ({before} → {after} messages)."
        )));
    }

    async fn cmd_providers(&mut self, arg: &str) {
        match arg {
            "apikey" => self.state.blocks.push(Block::Notice(
                "API key authentication is coming soon.".into(),
            )),
            "subscription anthropic" => self.state.blocks.push(Block::Notice(
                "Anthropic (Claude Pro/Max) is coming soon.".into(),
            )),
            "subscription codex connect" => {
                let line = if self.state.provider_connected {
                    "Already connected to Codex."
                } else {
                    "Quit and restart `pyxis`: the built-in onboarding will \
                     reconnect ChatGPT."
                };
                self.state.blocks.push(Block::Notice(line.into()));
            }
            // US-019: same sign-out path as `/logout`, so the two surfaces
            // cannot promise different things.
            "subscription codex disconnect" => {
                if !self.state.provider_connected {
                    self.state
                        .blocks
                        .push(Block::Notice("Already disconnected.".into()));
                    return;
                }
                match sign_out(&self.cfg.provider).await {
                    Ok(()) => {
                        self.state.provider_connected = false;
                        self.state.blocks.push(Block::Notice(format!(
                            "Disconnected from Codex (credential removed). Log in \
                             again before the next model call. {LOGOUT_SERVER_NOTE}"
                        )));
                    }
                    Err(err) => self
                        .state
                        .blocks
                        .push(Block::Error(format!("disconnect: {err}"))),
                }
            }
            "" | "subscription" | "subscription codex" => self.state.blocks.push(Block::Notice(
                "Choose a provider and then an action in the submenu.".into(),
            )),
            other => self
                .state
                .blocks
                .push(Block::Notice(format!("Unknown provider: {other}"))),
        }
    }
}

/// `/init` never overwrites an instruction file on its own: the confirmation is
/// the explicit `force` argument, decided BEFORE the turn starts. Leaving the
/// guard to the permission pipeline would not do, because `accept-edits` and
/// `auto` approve a write without asking anyone.
fn init_decision(workspace: &std::path::Path, arg: &str) -> InitDecision {
    let forced = arg.trim() == "force";
    match crate::context::instructions_file(workspace) {
        Some(name) if forced => InitDecision::Overwrite(name),
        Some(name) => InitDecision::Confirm(name),
        None => InitDecision::Bootstrap,
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
            agent_core::FileChange::Renamed => 'R',
        };
        // A rename names both ends: the destination alone would read as a file
        // appearing from nowhere.
        match &file.moved_from {
            Some(source) => report.push_str(&format!("\n  {mark} {source} -> {}", file.path)),
            None => report.push_str(&format!(
                "\n  {mark} {} +{} -{}",
                file.path, file.added_lines, file.removed_lines
            )),
        }
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

/// Raw text of the last assistant answer, as it was streamed (US-019 AC4): no
/// rendering, no markup added, and the goal marker removed like the headless
/// output does. `None` when no answer has been displayed yet.
fn last_assistant_text(state: &agent_tui::AppState) -> Option<String> {
    state.blocks.iter().rev().find_map(|block| match block {
        Block::Assistant { text, .. } => Some(
            text.replace(super::GOAL_DONE_MARKER, "")
                .trim_end()
                .to_string(),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tui::AppState;

    /// The picker lives in `agent-tui`, the handling here. Nothing links them at
    /// compile time, so a command added to one and not the other would reach the
    /// user as "Unknown command" from a menu that offered it.
    #[test]
    fn every_offered_command_has_a_handler() {
        let missing: Vec<&str> = COMMANDS
            .iter()
            .map(|(name, _, _)| *name)
            .filter(|name| !HANDLED_COMMANDS.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "offered by the picker with no arm in the dispatcher: {missing:?}"
        );
        let orphan: Vec<&&str> = HANDLED_COMMANDS
            .iter()
            .filter(|name| !COMMANDS.iter().any(|(offered, _, _)| offered == *name))
            .collect();
        assert!(
            orphan.is_empty(),
            "handled but never offered, hence undiscoverable: {orphan:?}"
        );
    }

    /// The guard used to be repeated at the head of four arms. It is now one
    /// table, and it must still cover exactly the commands that need a terminal
    /// turn boundary.
    #[test]
    fn the_idle_guard_covers_every_command_that_moves_the_conversation() {
        for command in ["/fork", "/rewind", "/resume", "/new", "/clear", "/compact"] {
            assert!(
                NEEDS_IDLE.contains(&command),
                "`{command}` works on the transcript: it needs a terminal boundary"
            );
        }
        // `/goal` and `/init` submit a turn of their own.
        assert!(NEEDS_IDLE.contains(&"/goal"));
        assert!(NEEDS_IDLE.contains(&"/init"));
        // Read-only surfaces never wait.
        for command in [
            "/status",
            "/usage",
            "/diff",
            "/copy",
            "/hooks",
            "/approvals",
        ] {
            assert!(!NEEDS_IDLE.contains(&command), "`{command}` reads only");
        }
        // Every entry is a command someone can actually type.
        for command in NEEDS_IDLE {
            assert!(HANDLED_COMMANDS.contains(command), "{command}");
        }
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

    fn tmp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis-us019-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
            text: format!("**bold** answer\n{}\n", super::super::GOAL_DONE_MARKER),
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
