//! Hooks: user commands run around a tool call (US-017 to US-019).
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

/// Lifecycle event a hook can watch. Two of them: the reference contract
/// declares a dozen, this version implements what surrounds a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
        }
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
            _ => None,
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
    /// Exact name of the watched tool. `None` = every tool.
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
}

impl std::fmt::Debug for CommandHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandHooks")
            .field("specs", &self.specs)
            .field("workspace", &self.workspace)
            .field("timeout", &self.timeout)
            .field("harden", &self.harden.as_ref().map(|_| "<fn>"))
            .field("notice", &self.notice.as_ref().map(|_| "<fn>"))
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
        }
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
        assert_eq!(HookEvent::parse("SessionStart"), None);
        assert_eq!(HookEvent::PreToolUse.name(), "PreToolUse");
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
