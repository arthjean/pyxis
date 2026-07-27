//! Hooks: user commands run around a tool call (US-017 to US-019) and around the
//! session lifecycle (US-017 of the parity PRD).
//!
//! Contract of the converging ecosystem (Claude Code): a JSON event on the hook's
//! standard input, a JSON decision on its standard output under
//! `hookSpecificOutput.permissionDecision`, exit code 2 meaning "blocked" with the
//! standard error carried back to the model.
//!
//! Two deliberate deviations, both fail-closed:
//!
//! 1. **A hook can only tighten.** `allow` is read as "no objection", never as a
//!    bypass of the confirmation. A hook is a veto, not a way to widen a
//!    perimeter, so it can never neutralize the taint defense (4.6) nor the
//!    baseline decision of a tool.
//! 2. **Any failure denies.** Missing executable, timeout, non-zero exit code,
//!    unreadable standard output, unknown decision: the tool call is refused
//!    (US-018 AC3). The reference contract lets some of these through; the
//!    project's fail-closed principle does not.
//!
//! A hook inherits the process sandbox: the Landlock confinement is applied
//! process-wide before the runtime exists, so a hook cannot write outside the
//! writable roots of the session. That is a documented consequence, not a bug.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::tool::{CommandHardener, MAX_TOOL_OUTPUT_BYTES, truncate_tail};

/// Wall-clock bound of one hook process (NFR: 5 s).
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// Standard output is the decision channel: bounded BEFORE being interpreted
/// (US-017 AC4). A JSON decision truncated by this bound stops parsing, hence
/// denies.
const MAX_HOOK_STDOUT: usize = 64_000;
/// Standard error is the reason channel: shorter bound, its content travels to
/// the model.
const MAX_HOOK_STDERR: usize = 4_000;
/// Cap of a reason shown to the user and sent to the model.
const MAX_REASON: usize = 500;
/// Cap of the prompt carried by a `UserPromptSubmit` event. A paste of a hundred
/// kilobytes is still a prompt; it is not a reason to make every hook read one.
const MAX_HOOK_PROMPT_BYTES: usize = 16_000;

/// Event a hook can watch: the two that surround a tool call, and the four of the
/// session lifecycle. The reference contract declares eleven; these six are the
/// ones something in Pyxis actually happens at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Stop,
}

/// Every declared event, for the messages that list them.
pub const HOOK_EVENT_NAMES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
];

impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
        }
    }

    /// Whether the event names a tool, hence whether a `matcher` means anything.
    /// A lifecycle event watches the session, not a tool: there is nothing for a
    /// matcher to select.
    pub fn is_tool_scoped(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }

    /// Reads the event name of the reference contract. Tolerant on case and on
    /// separators, strict on everything else: an unknown name is not an event.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw
            .trim()
            .to_ascii_lowercase()
            .replace(['_', '-', ' '], "")
            .as_str()
        {
            "pretooluse" | "pre" => Some(Self::PreToolUse),
            "posttooluse" | "post" => Some(Self::PostToolUse),
            "sessionstart" => Some(Self::SessionStart),
            "sessionend" => Some(Self::SessionEnd),
            "userpromptsubmit" => Some(Self::UserPromptSubmit),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// One occurrence of a lifecycle event, with the field the reference contract
/// carries for it. Passing the occurrence rather than the bare event is what
/// keeps the payload and the event name from drifting apart.
#[derive(Debug, Clone, Copy)]
pub enum Lifecycle<'a> {
    /// Before the first turn. `source` is `startup` or `resume`.
    SessionStart { source: &'a str },
    /// Before a submitted prompt becomes a turn.
    UserPromptSubmit { prompt: &'a str },
    /// After a turn ends and nothing follows it.
    Stop,
    /// After the session, whatever ended it.
    SessionEnd { reason: &'a str },
}

impl Lifecycle<'_> {
    pub fn event(self) -> HookEvent {
        match self {
            Self::SessionStart { .. } => HookEvent::SessionStart,
            Self::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            Self::Stop => HookEvent::Stop,
            Self::SessionEnd { .. } => HookEvent::SessionEnd,
        }
    }

    /// Whether a refusal still has something to stop. `Stop` and `SessionEnd` fire
    /// once the thing they observe is already over: like `PostToolUse`, they have
    /// no decision to make, and a hook that fails there is REPORTED to the human
    /// rather than denying a call that no longer exists. A `Stop` that could force
    /// the model to keep going would also be a hook widening the run, which the
    /// restrictive-only rule of this module forbids.
    fn gates(self) -> bool {
        matches!(
            self,
            Self::SessionStart { .. } | Self::UserPromptSubmit { .. }
        )
    }

    /// Event-specific field of the payload, named as the reference contract names
    /// it.
    fn field(self) -> Option<(&'static str, String)> {
        match self {
            Self::SessionStart { source } => Some(("source", source.to_string())),
            Self::SessionEnd { reason } => Some(("reason", reason.to_string())),
            Self::UserPromptSubmit { prompt } => {
                Some(("prompt", truncate_head(prompt, MAX_HOOK_PROMPT_BYTES)))
            }
            Self::Stop => None,
        }
    }
}

/// Decision of a `PreToolUse` hook as the pipeline applies it. RESTRICTIVE ONLY:
/// `NoObjection` leaves the permission decision untouched, it never turns an
/// `Ask` into an `Allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// The hook has no opinion (silent, or explicit `allow`).
    NoObjection,
    /// Forces a human confirmation, whatever the active mode (US-018 AC2).
    Ask(String),
    /// Refuses the call, whatever the active mode (US-018 AC1/AC4).
    Deny(String),
}

/// One declared hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub event: HookEvent,
    /// Exact name of the watched tool. `None` = every tool. Always `None` on a
    /// lifecycle event, which watches the session and not a tool.
    pub matcher: Option<String>,
    /// Executable run directly (no shell): the declaration is an argv, not a
    /// command line, so nothing is re-interpreted between the configuration and
    /// the process.
    pub command: String,
    pub args: Vec<String>,
}

impl HookSpec {
    pub fn matches(&self, tool: &str) -> bool {
        self.matcher.as_deref().is_none_or(|m| m == tool)
    }

    /// Name of the hook in a refusal (US-017 AC6).
    pub fn label(&self) -> &str {
        &self.command
    }
}

/// Result of an already executed tool, handed to a `PostToolUse` hook.
#[derive(Debug, Clone, Copy)]
pub struct HookToolResult<'a> {
    pub content: &'a str,
    pub is_error: bool,
}

/// Boundary between the dispatch pipeline and the hook engine. The `Registry`
/// knows nothing about processes; tests inject a scripted double.
#[async_trait]
pub trait Hooks: Send + Sync {
    /// Is at least one hook watching this tool for this event? The pipeline calls
    /// this FIRST: without a declaration nothing is prepared, nothing is cloned
    /// and no process is started (US-017 AC5).
    fn intercepts(&self, event: HookEvent, tool: &str) -> bool;

    /// Same question for an event that names no tool. Call sites of the lifecycle
    /// ask this before building a payload, so a session without hooks pays
    /// nothing.
    fn watches(&self, _event: HookEvent) -> bool {
        false
    }

    /// Runs the hooks of a lifecycle event. Returns `Deny` only for the events
    /// that still gate something (`SessionStart`, `UserPromptSubmit`); for the
    /// others a failure travels through the engine's notice channel and the
    /// decision stays `NoObjection`.
    async fn lifecycle(&self, _event: Lifecycle<'_>) -> HookDecision {
        HookDecision::NoObjection
    }

    /// Runs the hooks preceding a tool call and returns the strictest decision.
    async fn pre_tool_use(&self, tool: &str, input: &serde_json::Value) -> HookDecision;

    /// Runs the hooks following a tool call. Returns nothing: a later hook cannot
    /// change a result already handed to the model (US-019 AC4). A failure is
    /// reported through the engine's own notice channel.
    async fn post_tool_use(
        &self,
        tool: &str,
        input: &serde_json::Value,
        result: HookToolResult<'_>,
    );
}

/// No hook declared: the pipeline's default.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

#[async_trait]
impl Hooks for NoHooks {
    fn intercepts(&self, _event: HookEvent, _tool: &str) -> bool {
        false
    }
    async fn pre_tool_use(&self, _tool: &str, _input: &serde_json::Value) -> HookDecision {
        HookDecision::NoObjection
    }
    async fn post_tool_use(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _result: HookToolResult<'_>,
    ) {
    }
}

/// Where the engine reports what does not belong in the model's transcript: the
/// failure of a hook that runs AFTER the tool call. Injected by the binary, which
/// alone knows whether a TUI or a standard error stream is listening.
pub type HookNotice = Arc<dyn Fn(String) + Send + Sync>;

/// Hook engine running real processes.
pub struct CommandHooks {
    specs: Vec<HookSpec>,
    workspace: PathBuf,
    timeout: Duration,
    harden: Option<CommandHardener>,
    notice: Option<HookNotice>,
    /// Session the lifecycle events belong to. Empty when the caller has none to
    /// give, in which case the field is simply absent from the payload.
    session_id: String,
}

impl std::fmt::Debug for CommandHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandHooks")
            .field("specs", &self.specs)
            .field("workspace", &self.workspace)
            .field("timeout", &self.timeout)
            .field("harden", &self.harden.as_ref().map(|_| "<fn>"))
            .field("notice", &self.notice.as_ref().map(|_| "<fn>"))
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl CommandHooks {
    pub fn new(specs: Vec<HookSpec>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            specs,
            workspace: workspace.into(),
            timeout: DEFAULT_HOOK_TIMEOUT,
            harden: None,
            notice: None,
            session_id: String::new(),
        }
    }

    /// Session identifier carried by the lifecycle payloads.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Same hardening as the tool subprocesses (network through the allow-list
    /// proxy).
    #[must_use]
    pub fn with_hardener(mut self, harden: CommandHardener) -> Self {
        self.harden = Some(harden);
        self
    }

    #[must_use]
    pub fn with_notice(mut self, notice: HookNotice) -> Self {
        self.notice = Some(notice);
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn matching(&self, event: HookEvent) -> impl Iterator<Item = &HookSpec> {
        self.specs.iter().filter(move |s| s.event == event)
    }

    fn report(&self, message: String) {
        if let Some(notice) = &self.notice {
            notice(message);
        }
    }

    /// What a refusal means on a lifecycle event: a `Deny` for the events that
    /// still gate something, a notice for the ones that observe an already
    /// finished thing.
    fn lifecycle_refusal(&self, event: Lifecycle<'_>, reason: String) -> HookDecision {
        let message = format!("{}: {reason}", event.event().name());
        if event.gates() {
            HookDecision::Deny(message)
        } else {
            self.report(message);
            HookDecision::NoObjection
        }
    }

    /// Runs one hook to completion under the timeout. `Err` = the hook produced no
    /// usable verdict (spawn failure, timeout, unreadable status).
    async fn run(&self, spec: &HookSpec, payload: Vec<u8>) -> Result<HookExit, String> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(&self.workspace)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(not(windows))]
        cmd.process_group(0);
        // The Landlock confinement is process-wide, hence inherited; only the
        // network hardening has to be re-applied per subprocess.
        if let Some(harden) = &self.harden {
            harden(&mut cmd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("hook `{}` could not start: {e}", spec.label()))?;
        let pid = child.id();

        // The event is written on a task: a hook that answers without reading its
        // input must not deadlock the writer on a full pipe.
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin.write_all(&payload).await;
                let _ = stdin.shutdown().await;
            });
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(async move { read_bounded(stdout, MAX_HOOK_STDOUT).await });
        let err_task = tokio::spawn(async move { read_bounded(stderr, MAX_HOOK_STDERR).await });

        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                out_task.abort();
                err_task.abort();
                return Err(format!("hook `{}` could not be awaited: {e}", spec.label()));
            }
            Err(_) => {
                if let Some(pid) = pid {
                    crate::bash::kill_process_tree(pid).await;
                }
                let _ = child.kill().await;
                out_task.abort();
                err_task.abort();
                return Err(format!(
                    "hook `{}` timed out after {} s",
                    spec.label(),
                    self.timeout.as_secs()
                ));
            }
        };

        Ok(HookExit {
            code: status.code(),
            stdout: out_task.await.unwrap_or_default(),
            stderr: err_task.await.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl Hooks for CommandHooks {
    fn intercepts(&self, event: HookEvent, tool: &str) -> bool {
        self.matching(event).any(|spec| spec.matches(tool))
    }

    fn watches(&self, event: HookEvent) -> bool {
        self.matching(event).next().is_some()
    }

    async fn lifecycle(&self, event: Lifecycle<'_>) -> HookDecision {
        let kind = event.event();
        // US-017 AC6: no declaration, no payload, no process.
        if !self.watches(kind) {
            return HookDecision::NoObjection;
        }
        let payload = match serde_json::to_vec(&lifecycle_payload(
            event,
            &self.session_id,
            &self.workspace,
        )) {
            Ok(bytes) => bytes,
            Err(e) => {
                return self.lifecycle_refusal(event, format!("event not serializable: {e}"));
            }
        };
        for spec in self.matching(kind) {
            let decision = match self.run(spec, payload.clone()).await {
                Ok(exit) => lifecycle_decision(spec.label(), &exit),
                // Missing executable, timeout: fail-closed, as everywhere else.
                Err(failure) => HookDecision::Deny(failure),
            };
            // A refusal ends the sequence: nothing left to gate, and the
            // remaining hooks would only cost time.
            if let HookDecision::Deny(reason) = decision {
                return self.lifecycle_refusal(event, reason);
            }
        }
        HookDecision::NoObjection
    }

    async fn pre_tool_use(&self, tool: &str, input: &serde_json::Value) -> HookDecision {
        let payload = match serde_json::to_vec(&payload(
            HookEvent::PreToolUse,
            tool,
            input,
            None,
            &self.workspace,
        )) {
            Ok(bytes) => bytes,
            // Fail-closed: an event we cannot even serialize is not an event a
            // hook could have approved.
            Err(e) => return HookDecision::Deny(format!("hook event not serializable: {e}")),
        };

        let mut forced_ask: Option<String> = None;
        for spec in self.matching(HookEvent::PreToolUse) {
            if !spec.matches(tool) {
                continue;
            }
            let decision = match self.run(spec, payload.clone()).await {
                Ok(exit) => interpret(spec.label(), &exit),
                // Missing executable, timeout: the call is refused (US-017 AC6,
                // US-018 AC3).
                Err(failure) => HookDecision::Deny(failure),
            };
            match decision {
                // Strictest wins, and a refusal ends the sequence: no reason to
                // pay for the remaining hooks once the call is dead.
                HookDecision::Deny(reason) => return HookDecision::Deny(reason),
                HookDecision::Ask(reason) => {
                    forced_ask.get_or_insert(reason);
                }
                HookDecision::NoObjection => {}
            }
        }
        match forced_ask {
            Some(reason) => HookDecision::Ask(reason),
            None => HookDecision::NoObjection,
        }
    }

    async fn post_tool_use(
        &self,
        tool: &str,
        input: &serde_json::Value,
        result: HookToolResult<'_>,
    ) {
        let payload = match serde_json::to_vec(&payload(
            HookEvent::PostToolUse,
            tool,
            input,
            Some(result),
            &self.workspace,
        )) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.report(format!("hook event not serializable: {e}"));
                return;
            }
        };
        for spec in self.matching(HookEvent::PostToolUse) {
            if !spec.matches(tool) {
                continue;
            }
            // A failure here is reported, never fatal: the tool has already run and
            // its result already belongs to the model (US-019 AC2).
            match self.run(spec, payload.clone()).await {
                Ok(exit) => match exit.code {
                    Some(0) => {}
                    Some(n) => self.report(format!(
                        "hook `{}` exited with code {n}{}",
                        spec.label(),
                        detail(&exit.stderr)
                    )),
                    None => self.report(format!(
                        "hook `{}` was terminated by a signal",
                        spec.label()
                    )),
                },
                Err(failure) => self.report(failure),
            }
        }
    }
}

/// What one hook process produced.
#[derive(Debug, Clone, Default)]
pub struct HookExit {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Reads the decision of a finished hook. PURE (no I/O) -> unit-testable without
/// spawning anything.
///
/// - exit 0 + empty output = no opinion, the pipeline is untouched;
/// - exit 0 + JSON decision = that decision, `allow` meaning "no objection";
/// - exit 0 + unreadable output = refusal (the decision channel is not a log);
/// - exit 2 = refusal with the standard error as its reason;
/// - any other end = refusal.
pub fn interpret(label: &str, exit: &HookExit) -> HookDecision {
    match exit.code {
        Some(0) => interpret_stdout(label, &exit.stdout),
        Some(2) => HookDecision::Deny(match short(exit.stderr.trim()) {
            reason if reason.is_empty() => format!("hook `{label}` blocked the call"),
            reason => format!("hook `{label}`: {reason}"),
        }),
        Some(n) => HookDecision::Deny(format!(
            "hook `{label}` failed (exit code {n}){}",
            detail(&exit.stderr)
        )),
        None => HookDecision::Deny(format!("hook `{label}` was terminated by a signal")),
    }
}

fn interpret_stdout(label: &str, stdout: &str) -> HookDecision {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return HookDecision::NoObjection;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Bounded, hence possibly truncated: an unreadable decision is not a
        // decision. Hooks log on their standard error.
        return HookDecision::Deny(format!(
            "hook `{label}`: unreadable decision on stdout (JSON expected)"
        ));
    };
    let specific = value.get("hookSpecificOutput");
    let decision = specific
        .and_then(|s| s.get("permissionDecision"))
        .and_then(serde_json::Value::as_str);
    let reason = specific
        .and_then(|s| s.get("permissionDecisionReason"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(short);
    match decision {
        // Valid JSON carrying no decision: the hook observed without objecting.
        None => HookDecision::NoObjection,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            // Read as "no objection": a hook tightens, it never widens.
            "allow" => HookDecision::NoObjection,
            "ask" => HookDecision::Ask(
                reason.unwrap_or_else(|| format!("hook `{label}` requires a confirmation")),
            ),
            "deny" => HookDecision::Deny(match reason {
                Some(reason) => format!("hook `{label}`: {reason}"),
                None => format!("hook `{label}` refused the call"),
            }),
            other => HookDecision::Deny(format!(
                "hook `{label}`: unknown decision `{}`",
                short(other)
            )),
        },
    }
}

/// Reads the decision of a finished LIFECYCLE hook. Same reading as a tool hook,
/// except that `ask` has nowhere to go: a lifecycle event carries no per-call
/// confirmation, so a hook asking for one gets the fail-closed answer rather than
/// a silent pass.
fn lifecycle_decision(label: &str, exit: &HookExit) -> HookDecision {
    match interpret(label, exit) {
        HookDecision::Ask(reason) => HookDecision::Deny(format!(
            "hook `{label}`: `ask` has no confirmation to request on a lifecycle event ({reason})"
        )),
        other => other,
    }
}

/// The lifecycle event handed to a hook on its standard input.
fn lifecycle_payload(event: Lifecycle<'_>, session_id: &str, cwd: &Path) -> serde_json::Value {
    let mut value = serde_json::json!({
        "hook_event_name": event.event().name(),
        "cwd": cwd.display().to_string(),
    });
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    if !session_id.is_empty() {
        object.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
    }
    if let Some((key, field)) = event.field() {
        object.insert(key.to_string(), serde_json::Value::String(field));
    }
    value
}

/// The event handed to a hook on its standard input.
fn payload(
    event: HookEvent,
    tool: &str,
    input: &serde_json::Value,
    result: Option<HookToolResult<'_>>,
    cwd: &Path,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "hook_event_name": event.name(),
        "tool_name": tool,
        "tool_input": input,
        "cwd": cwd.display().to_string(),
    });
    if let Some(result) = result
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "tool_response".to_string(),
            serde_json::json!({
                // Same truncation policy as the other tool outputs (US-019 AC3).
                "content": truncate_tail(result.content, MAX_TOOL_OUTPUT_BYTES),
                "is_error": result.is_error,
            }),
        );
    }
    value
}

/// Reads a stream to EOF but keeps only its HEAD: a decision sits at the start of
/// the output, and draining the rest keeps the child from blocking on a full pipe.
async fn read_bounded(stream: Option<impl tokio::io::AsyncRead + Unpin>, max: usize) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < max {
                    let room = max - kept.len();
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

fn detail(stderr: &str) -> String {
    match short(stderr.trim()) {
        reason if reason.is_empty() => String::new(),
        reason => format!(": {reason}"),
    }
}

/// Bounds a payload field, keeping its HEAD: a prompt says what it is about at
/// the start, and a hook reading it needs a predictable size.
fn truncate_head(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Bounds a reason before it reaches a dialog or the model.
fn short(reason: &str) -> String {
    if reason.len() <= MAX_REASON {
        return reason.to_string();
    }
    let mut end = MAX_REASON;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &reason[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(code: i32, stdout: &str, stderr: &str) -> HookExit {
        HookExit {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn decision(value: &str) -> String {
        format!(
            r#"{{"hookSpecificOutput":{{"permissionDecision":"{value}","permissionDecisionReason":"parce que"}}}}"#
        )
    }

    /// Reason of a refusal, empty when the decision is not one: lets an assert
    /// carry the whole decision in its message instead of panicking apart.
    fn deny_reason(decision: &HookDecision) -> &str {
        match decision {
            HookDecision::Deny(reason) => reason.as_str(),
            _ => "",
        }
    }

    #[test]
    fn silent_success_leaves_the_pipeline_untouched() {
        assert_eq!(
            interpret("guard", &exit(0, "  \n", "")),
            HookDecision::NoObjection
        );
    }

    #[test]
    fn allow_is_read_as_no_objection_not_as_a_bypass() {
        // A hook tightens, it never widens: `allow` must not turn an `Ask` into
        // an `Allow` further down the pipeline.
        assert_eq!(
            interpret("guard", &exit(0, &decision("allow"), "")),
            HookDecision::NoObjection
        );
    }

    #[test]
    fn ask_and_deny_carry_their_reason() {
        assert_eq!(
            interpret("guard", &exit(0, &decision("ask"), "")),
            HookDecision::Ask("parce que".to_string())
        );
        assert_eq!(
            interpret("guard", &exit(0, &decision("deny"), "")),
            HookDecision::Deny("hook `guard`: parce que".to_string())
        );
    }

    #[test]
    fn decision_matching_is_case_insensitive() {
        assert_eq!(
            interpret("guard", &exit(0, &decision("DENY"), "")),
            HookDecision::Deny("hook `guard`: parce que".to_string())
        );
    }

    /// US-017 AC2 / edge case 20: an unforeseen value is a refusal, never a pass.
    #[test]
    fn unknown_decision_denies() {
        let decision = interpret("guard", &exit(0, &decision("maybe"), ""));
        assert!(deny_reason(&decision).contains("maybe"), "{decision:?}");
    }

    #[test]
    fn unreadable_stdout_denies() {
        assert!(matches!(
            interpret("guard", &exit(0, "not json at all", "")),
            HookDecision::Deny(_)
        ));
    }

    #[test]
    fn json_without_decision_is_no_objection() {
        assert_eq!(
            interpret("guard", &exit(0, r#"{"systemMessage":"fyi"}"#, "")),
            HookDecision::NoObjection
        );
    }

    #[test]
    fn exit_two_blocks_with_stderr_as_reason() {
        assert_eq!(
            interpret("guard", &exit(2, "", "chemin interdit")),
            HookDecision::Deny("hook `guard`: chemin interdit".to_string())
        );
    }

    #[test]
    fn any_other_failure_denies() {
        assert!(matches!(
            interpret("guard", &exit(1, "", "boom")),
            HookDecision::Deny(_)
        ));
        assert!(matches!(
            interpret(
                "guard",
                &HookExit {
                    code: None,
                    ..HookExit::default()
                }
            ),
            HookDecision::Deny(_)
        ));
    }

    #[test]
    fn reasons_are_bounded() {
        let long = "x".repeat(MAX_REASON * 3);
        let decision = interpret("guard", &exit(2, "", &long));
        let reason = deny_reason(&decision);
        assert!(!reason.is_empty(), "exit 2 doit refuser: {decision:?}");
        assert!(reason.len() < MAX_REASON * 2, "raison non bornée");
    }

    /// US-017 AC5: without a declaration the pipeline asks one boolean and stops
    /// there, so nothing is prepared and no process is started.
    #[tokio::test]
    async fn no_hooks_intercepts_nothing() {
        assert!(!NoHooks.intercepts(HookEvent::PreToolUse, "bash"));
        assert!(!NoHooks.intercepts(HookEvent::PostToolUse, "bash"));
        assert_eq!(
            NoHooks.pre_tool_use("bash", &serde_json::json!({})).await,
            HookDecision::NoObjection
        );
    }

    /// A declaration watching another event does not intercept this one.
    #[test]
    fn a_declaration_is_scoped_to_its_event() {
        let hooks = CommandHooks::new(
            vec![HookSpec {
                event: HookEvent::PostToolUse,
                matcher: None,
                command: "/bin/true".to_string(),
                args: Vec::new(),
            }],
            std::env::temp_dir(),
        );
        assert!(!hooks.intercepts(HookEvent::PreToolUse, "bash"));
        assert!(hooks.intercepts(HookEvent::PostToolUse, "bash"));
    }

    #[test]
    fn event_names_follow_the_reference_contract() {
        assert_eq!(HookEvent::parse("PreToolUse"), Some(HookEvent::PreToolUse));
        assert_eq!(
            HookEvent::parse("post_tool_use"),
            Some(HookEvent::PostToolUse)
        );
        assert_eq!(HookEvent::parse("Rewind"), None);
        assert_eq!(HookEvent::PreToolUse.name(), "PreToolUse");
    }

    /// AC1: the four lifecycle events join the two existing ones, under the names
    /// of the reference contract.
    #[test]
    fn the_lifecycle_events_are_declared_and_readable() {
        for name in HOOK_EVENT_NAMES {
            assert_eq!(
                HookEvent::parse(name).map(HookEvent::name),
                Some(*name),
                "{name} unreadable"
            );
        }
        assert_eq!(HOOK_EVENT_NAMES.len(), 6);
        assert_eq!(
            HookEvent::parse("session-start"),
            Some(HookEvent::SessionStart)
        );
        // A lifecycle event watches the session: a matcher would have nothing to
        // select.
        assert!(HookEvent::PreToolUse.is_tool_scoped());
        assert!(!HookEvent::SessionStart.is_tool_scoped());
        assert!(!HookEvent::Stop.is_tool_scoped());
    }

    #[test]
    fn a_lifecycle_payload_carries_the_field_of_its_event() {
        let start = lifecycle_payload(
            Lifecycle::SessionStart { source: "resume" },
            "2026-07-27.jsonl",
            Path::new("/ws"),
        );
        assert_eq!(start["hook_event_name"], "SessionStart");
        assert_eq!(start["session_id"], "2026-07-27.jsonl");
        assert_eq!(start["cwd"], "/ws");
        assert_eq!(start["source"], "resume");

        let stop = lifecycle_payload(Lifecycle::Stop, "", Path::new("/ws"));
        assert_eq!(stop["hook_event_name"], "Stop");
        // No session to name, no key: a hook reads an absence, never an empty
        // string it would have to interpret.
        assert!(stop.get("session_id").is_none());
        assert!(stop.get("source").is_none());

        let long = "p".repeat(MAX_HOOK_PROMPT_BYTES * 2);
        let submit = lifecycle_payload(
            Lifecycle::UserPromptSubmit { prompt: &long },
            "s",
            Path::new("/ws"),
        );
        let carried = submit["prompt"].as_str().unwrap();
        assert!(
            carried.len() <= MAX_HOOK_PROMPT_BYTES + 8,
            "prompt unbounded"
        );
        assert!(carried.ends_with('…'));
    }

    /// AC6: no declaration, nothing runs.
    #[tokio::test]
    async fn a_session_without_hooks_watches_nothing() {
        assert!(!NoHooks.watches(HookEvent::SessionStart));
        assert_eq!(
            NoHooks
                .lifecycle(Lifecycle::UserPromptSubmit { prompt: "hello" })
                .await,
            HookDecision::NoObjection
        );
        let hooks = engine(vec![shell_hook(HookEvent::PreToolUse, "exit 2")]);
        assert!(!hooks.watches(HookEvent::UserPromptSubmit));
        assert_eq!(
            hooks
                .lifecycle(Lifecycle::UserPromptSubmit { prompt: "hello" })
                .await,
            HookDecision::NoObjection
        );
    }

    /// AC2: a `UserPromptSubmit` hook that refuses stops the turn, and the reason
    /// names both the event and the hook.
    #[tokio::test]
    async fn a_user_prompt_submit_hook_can_refuse_the_turn() {
        let hooks = engine(vec![shell_hook(
            HookEvent::UserPromptSubmit,
            r#"grep -q '"prompt":"deploy prod"' && { echo "pas en prod" >&2; exit 2; }"#,
        )]);
        let decision = hooks
            .lifecycle(Lifecycle::UserPromptSubmit {
                prompt: "deploy prod",
            })
            .await;
        let reason = deny_reason(&decision);
        assert!(reason.contains("UserPromptSubmit"), "{decision:?}");
        assert!(reason.contains("pas en prod"), "{decision:?}");
    }

    /// AC3: failure, timeout and unknown decision all deny on a gating event.
    #[tokio::test]
    async fn a_gating_lifecycle_hook_fails_closed() {
        let missing = CommandHooks::new(
            vec![HookSpec {
                event: HookEvent::SessionStart,
                matcher: None,
                command: "/nonexistent/pyxis-hook".to_string(),
                args: Vec::new(),
            }],
            std::env::temp_dir(),
        );
        assert!(matches!(
            missing
                .lifecycle(Lifecycle::SessionStart { source: "startup" })
                .await,
            HookDecision::Deny(_)
        ));

        let hanging = engine(vec![shell_hook(HookEvent::SessionStart, "sleep 30")])
            .with_timeout(Duration::from_millis(150));
        assert!(
            deny_reason(
                &hanging
                    .lifecycle(Lifecycle::SessionStart { source: "startup" })
                    .await
            )
            .contains("timed out")
        );

        let unknown = engine(vec![shell_hook(
            HookEvent::SessionStart,
            r#"printf '{"hookSpecificOutput":{"permissionDecision":"maybe"}}'"#,
        )]);
        assert!(matches!(
            unknown
                .lifecycle(Lifecycle::SessionStart { source: "startup" })
                .await,
            HookDecision::Deny(_)
        ));
    }

    /// A lifecycle event has no per-call confirmation to request, so `ask` is
    /// answered fail-closed instead of being read as a pass.
    #[tokio::test]
    async fn ask_on_a_lifecycle_event_is_a_refusal() {
        let hooks = engine(vec![shell_hook(
            HookEvent::UserPromptSubmit,
            r#"printf '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"prudence"}}'"#,
        )]);
        let decision = hooks
            .lifecycle(Lifecycle::UserPromptSubmit { prompt: "go" })
            .await;
        assert!(
            deny_reason(&decision).contains("no confirmation"),
            "{decision:?}"
        );
    }

    /// AC4: `allow` stays "no objection" on the lifecycle too.
    #[tokio::test]
    async fn allow_on_a_lifecycle_event_stays_no_objection() {
        let hooks = engine(vec![shell_hook(
            HookEvent::SessionStart,
            r#"printf '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#,
        )]);
        assert_eq!(
            hooks
                .lifecycle(Lifecycle::SessionStart { source: "startup" })
                .await,
            HookDecision::NoObjection
        );
    }

    /// `Stop` and `SessionEnd` observe something already over: a failure is
    /// reported to the human, and nothing is denied.
    #[tokio::test]
    async fn an_observing_lifecycle_hook_reports_instead_of_denying() {
        let notices = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&notices);
        let hooks = engine(vec![
            shell_hook(HookEvent::Stop, "echo journal illisible >&2; exit 1"),
            shell_hook(HookEvent::SessionEnd, "exit 2"),
        ])
        .with_notice(Arc::new(move |msg| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
        }));

        assert_eq!(
            hooks.lifecycle(Lifecycle::Stop).await,
            HookDecision::NoObjection
        );
        assert_eq!(
            hooks
                .lifecycle(Lifecycle::SessionEnd { reason: "exit" })
                .await,
            HookDecision::NoObjection
        );
        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].starts_with("Stop: "), "{notices:?}");
        assert!(notices[0].contains("journal illisible"), "{notices:?}");
        assert!(notices[1].starts_with("SessionEnd: "), "{notices:?}");
    }

    /// The session identifier reaches the hook, so a lifecycle command can find
    /// the transcript it is about.
    #[tokio::test]
    async fn a_lifecycle_hook_reads_the_session_on_its_standard_input() {
        let hooks = CommandHooks::new(
            vec![shell_hook(
                HookEvent::SessionStart,
                r#"p=$(cat); case "$p" in *'"session_id":"abc.jsonl"'*) ;; *) exit 2;; esac; case "$p" in *'"source":"resume"'*) ;; *) exit 2;; esac"#,
            )],
            std::env::temp_dir(),
        )
        .with_session_id("abc.jsonl");
        assert_eq!(
            hooks
                .lifecycle(Lifecycle::SessionStart { source: "resume" })
                .await,
            HookDecision::NoObjection
        );
    }

    #[test]
    fn matcher_absent_watches_every_tool() {
        let spec = HookSpec {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "guard".into(),
            args: Vec::new(),
        };
        assert!(spec.matches("bash"));
        assert!(spec.matches("read"));

        let scoped = HookSpec {
            matcher: Some("bash".into()),
            ..spec
        };
        assert!(scoped.matches("bash"));
        assert!(!scoped.matches("read"));
    }

    #[test]
    fn payload_carries_the_event_the_tool_and_its_input() {
        let input = serde_json::json!({ "command": "ls" });
        let event = payload(
            HookEvent::PreToolUse,
            "bash",
            &input,
            None,
            Path::new("/ws"),
        );
        assert_eq!(event["hook_event_name"], "PreToolUse");
        assert_eq!(event["tool_name"], "bash");
        assert_eq!(event["tool_input"]["command"], "ls");
        assert_eq!(event["cwd"], "/ws");
        assert!(event.get("tool_response").is_none());
    }

    #[test]
    fn post_payload_carries_a_bounded_result() {
        let content = "z".repeat(MAX_TOOL_OUTPUT_BYTES * 2);
        let event = payload(
            HookEvent::PostToolUse,
            "bash",
            &serde_json::json!({}),
            Some(HookToolResult {
                content: &content,
                is_error: true,
            }),
            Path::new("/ws"),
        );
        assert_eq!(event["tool_response"]["is_error"], true);
        let rendered = event["tool_response"]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            rendered.len() <= MAX_TOOL_OUTPUT_BYTES + 100,
            "résultat non borné"
        );
        assert!(rendered.starts_with("[... output truncated"));
    }

    // ───────────── real processes (Linux, ADR-11) ─────────────

    fn shell_hook(event: HookEvent, script: &str) -> HookSpec {
        HookSpec {
            event,
            matcher: None,
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
        }
    }

    fn engine(specs: Vec<HookSpec>) -> CommandHooks {
        CommandHooks::new(specs, std::env::temp_dir())
    }

    #[tokio::test]
    async fn a_hook_receives_the_event_on_its_standard_input() {
        // The hook denies only when it actually read the tool name on stdin.
        let hooks = engine(vec![shell_hook(
            HookEvent::PreToolUse,
            r#"grep -q '"tool_name":"bash"' && printf '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"vu"}}'"#,
        )]);
        let decision = hooks
            .pre_tool_use("bash", &serde_json::json!({ "command": "ls" }))
            .await;
        assert!(
            matches!(&decision, HookDecision::Deny(r) if r.contains("vu")),
            "{decision:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_executable_denies_and_names_the_hook() {
        // US-017 AC6.
        let hooks = engine(vec![HookSpec {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "/nonexistent/pyxis-hook".to_string(),
            args: Vec::new(),
        }]);
        let decision = hooks.pre_tool_use("bash", &serde_json::json!({})).await;
        assert!(
            deny_reason(&decision).contains("/nonexistent/pyxis-hook"),
            "{decision:?}"
        );
    }

    #[tokio::test]
    async fn a_hook_that_hangs_times_out_and_denies() {
        // US-018 AC3 / edge case 19.
        let hooks = engine(vec![shell_hook(HookEvent::PreToolUse, "sleep 30")])
            .with_timeout(Duration::from_millis(150));
        let decision = hooks.pre_tool_use("bash", &serde_json::json!({})).await;
        assert!(deny_reason(&decision).contains("timed out"), "{decision:?}");
    }

    #[tokio::test]
    async fn a_flooding_hook_is_bounded_and_denies() {
        // US-017 AC4: the output is bounded BEFORE interpretation, so a decision
        // drowned in noise is unreadable, hence refused.
        let hooks = engine(vec![shell_hook(
            HookEvent::PreToolUse,
            "yes 0123456789abcdef | head -c 400000",
        )]);
        assert!(matches!(
            hooks.pre_tool_use("bash", &serde_json::json!({})).await,
            HookDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn only_matching_hooks_run() {
        let hooks = engine(vec![HookSpec {
            matcher: Some("write".to_string()),
            ..shell_hook(HookEvent::PreToolUse, "exit 2")
        }]);
        assert!(!hooks.intercepts(HookEvent::PreToolUse, "bash"));
        assert!(hooks.intercepts(HookEvent::PreToolUse, "write"));
        assert_eq!(
            hooks.pre_tool_use("bash", &serde_json::json!({})).await,
            HookDecision::NoObjection
        );
        assert!(matches!(
            hooks.pre_tool_use("write", &serde_json::json!({})).await,
            HookDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn the_strictest_decision_wins_over_the_declaration_order() {
        let hooks = engine(vec![
            shell_hook(
                HookEvent::PreToolUse,
                r#"printf '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"prudence"}}'"#,
            ),
            shell_hook(HookEvent::PreToolUse, "echo interdit >&2; exit 2"),
        ]);
        assert!(matches!(
            hooks.pre_tool_use("bash", &serde_json::json!({})).await,
            HookDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn a_failing_post_hook_is_reported_not_fatal() {
        // US-019 AC2.
        let notices = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&notices);
        let hooks = engine(vec![shell_hook(
            HookEvent::PostToolUse,
            "echo formatage impossible >&2; exit 1",
        )])
        .with_notice(Arc::new(move |msg| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
        }));

        hooks
            .post_tool_use(
                "bash",
                &serde_json::json!({ "command": "ls" }),
                HookToolResult {
                    content: "ok",
                    is_error: false,
                },
            )
            .await;

        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("formatage impossible"), "{notices:?}");
    }
}
