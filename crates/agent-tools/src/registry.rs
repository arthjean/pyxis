//! `Registry`: implements `ToolDispatch` (the core <-> tools boundary). Runs
//! each batch in ordered segments: safe reads in parallel, mutations and
//! confirmations serially, then puts every call through the **strict pipeline** (4.3):
//!
//! ```text
//! parse+validate -> hooks PreToolUse -> permission(mode x taint) -> call() under
//! timeout -> taint -> hooks PostToolUse -> outcome
//! ```
//!
//! Invariants: one `ToolOutcome` per `ToolInvocation` (even a refusal/unknown/failed
//! parse -> an error outcome, never a panic, correlation by `id`), fail-closed
//! everywhere.

use std::collections::HashMap;
use std::sync::Arc;

use agent_core::event::PermissionReq;
use agent_core::message::ToolErrorKind;
use agent_core::provider::ToolSpec;
use agent_core::tools::{
    ToolDispatch, ToolDispatchEvent, ToolEventSink, ToolInvocation, ToolOutcome,
};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};

use crate::error::ToolError;
use crate::hooks::{HookDecision, HookEvent, HookToolResult, Hooks, NoHooks};
use crate::permission::{
    ApprovalKey, ApprovalMemo, ApprovalMemory, Approver, AutoDeny, PermCtx, PermissionMode,
    PermissionModeState, PermissionRequest, Resolved, resolve_permission,
};
use crate::taint::TaintTracker;
use crate::tool::{DynTool, ToolCtx, into_dyn};

/// Tool description capped at exposure time (a tool does not pollute the prompt).
const MAX_DESCRIPTION: usize = 2048;
/// Reason shown when the taint defense forbids remembering an answer (US-008 AC5).
const TAINT_NOT_MEMOIZABLE: &str = "untrusted content was read in this turn";
/// Reason shown when a hook is what forces the confirmation (US-018 AC2): a
/// remembered answer would silence that hook for the rest of the session.
const HOOK_NOT_MEMOIZABLE: &str = "the confirmation comes from a hook";
/// Concurrency cap of the read-only batch (ARCHITECTURE 4.2).
const CONCURRENCY: usize = 10;

/// Tool registry + execution policy. Built by the CLI/TUI, injected
/// into the core as `Arc<dyn ToolDispatch>`.
pub struct Registry {
    tools: HashMap<String, Box<dyn DynTool>>,
    mode: PermissionModeState,
    approver: Arc<dyn Approver>,
    approvals: ApprovalMemory,
    taint: TaintTracker,
    hooks: Arc<dyn Hooks>,
    ctx: ToolCtx,
}

impl Registry {
    pub fn builder(workspace: impl Into<std::path::PathBuf>) -> RegistryBuilder {
        RegistryBuilder {
            tools: HashMap::new(),
            mode: PermissionModeState::default(),
            approver: None,
            approvals: ApprovalMemory::new(),
            taint_window: crate::taint::DEFAULT_WINDOW,
            initial_taint_recent: false,
            hooks: None,
            ctx: ToolCtx::new(workspace),
        }
    }

    /// Answers remembered for this session (US-009 inspection surface).
    pub fn approvals(&self) -> ApprovalMemory {
        self.approvals.clone()
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode.get()
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        self.mode.set(mode);
    }

    /// Is the taint recent? (exposed for tests / observability.)
    pub fn taint_recent(&self) -> bool {
        self.taint.is_recent()
    }

    pub fn seed_taint(&self, recent_untrusted: bool) {
        if recent_untrusted {
            self.taint.seed_recent();
        }
    }

    /// Specs exposed to the model (capped descriptions), for `AgentContext.tools`.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|t| {
                let raw_description = t.description();
                let description =
                    truncate_utf8_prefix(&raw_description, MAX_DESCRIPTION).to_string();
                ToolSpec {
                    name: t.name().to_string(),
                    description,
                    input_schema: t.input_schema(),
                }
            })
            .collect();
        // Stable order (prompt / test determinism).
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Collects the behavioral guidelines of every tool (US-026), for
    /// injection into the system prompt. Deterministic order (sorted by tool name,
    /// tool guidelines in declaration order) -> stable and cache-friendly
    /// prompt.
    pub fn behavioral_guidelines(&self) -> Vec<String> {
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        let mut out = Vec::new();
        for n in names {
            if let Some(t) = self.tools.get(n) {
                for g in t.behavioral_guidelines() {
                    out.push((*g).to_string());
                }
            }
        }
        out
    }

    /// Convenience path for tests and direct calls into the registry.
    pub async fn dispatch(&self, calls: Vec<ToolInvocation>) -> Vec<ToolOutcome> {
        <Self as ToolDispatch>::dispatch(self, calls, ToolEventSink::default()).await
    }

    /// Strict pipeline of a single call. Never panics: always returns a
    /// `ToolOutcome` correlated by `id`.
    async fn run_one(&self, call: ToolInvocation, events: ToolEventSink) -> ToolOutcome {
        let id = call.id.clone();
        let Some(tool) = self.tools.get(&call.name) else {
            return err_outcome(
                id,
                format!("unknown tool: {}", call.name),
                ToolErrorKind::UnknownTool,
            );
        };

        // 1. parse + validate (fail-closed, US-010 AC3): no execution on failure.
        if let Err(e) = tool.precheck(&call.input, &self.ctx) {
            return err_outcome(id, e.to_string(), e.kind());
        }

        // 2. hooks PreToolUse (4.3): a user rule runs BEFORE the permission
        // decision and can only TIGHTEN it. A refusal returns here, hence ahead
        // of every mode, including `BypassPermissions` (US-018 AC4).
        let mut hook_ask: Option<String> = None;
        if self.hooks.intercepts(HookEvent::PreToolUse, &call.name) {
            match self.hooks.pre_tool_use(&call.name, &call.input).await {
                HookDecision::Deny(reason) => {
                    return err_outcome(
                        id,
                        // The reason already names the hook when it comes from the
                        // process engine (US-017 AC6): no need to say it twice.
                        format!(
                            "action \"{}\" refused before execution: {reason}",
                            call.name
                        ),
                        ToolErrorKind::PermissionDenied,
                    );
                }
                HookDecision::Ask(reason) => hook_ask = Some(reason),
                HookDecision::NoObjection => {}
            }
        }

        // 3. permission: tool baseline shaped by mode + taint (4.4/4.6).
        let mode = self.mode();
        let pctx = PermCtx {
            mode,
            taint_recent: self.taint.is_recent(),
        };
        let baseline = tool.permission(&call.input, &pctx);
        let resolved = resolve_permission(
            mode,
            baseline,
            tool.is_read_only(),
            tool.is_sensitive(),
            tool.is_taint_sensitive(),
            pctx.taint_recent,
        );
        // A hook that asks turns an authorization into a question, in every mode
        // (US-018 AC2). It never turns a refusal into a question.
        let resolved = match resolved {
            Resolved::Deny => Resolved::Deny,
            _ if hook_ask.is_some() => Resolved::Ask,
            other => other,
        };
        // US-020: what the pipeline decided, without the arguments. The input can
        // carry a file body or a command: it only appears at the highest verbosity
        // (AC6), never at `debug`.
        tracing::debug!(
            target: "pyxis::tools",
            tool = %call.name,
            ?mode,
            ?resolved,
            taint_recent = pctx.taint_recent,
            "tool permission resolved"
        );
        tracing::trace!(
            target: "pyxis::tools",
            tool = %call.name,
            input = %summarize(&call.input),
            "tool input"
        );
        match resolved {
            Resolved::Deny => {
                return err_outcome(
                    id,
                    format!("permission denied for \"{}\" (mode {:?})", call.name, mode),
                    ToolErrorKind::PermissionDenied,
                );
            }
            Resolved::Ask => {
                let taint_forced = pctx.taint_recent && tool.is_taint_sensitive();
                // US-008: a remembered answer applies only outside a tainted
                // context. The taint defense outranks the memory, in both
                // directions: it re-asks, and it forbids remembering.
                let (key, memo_refused) = if taint_forced {
                    (None, Some(TAINT_NOT_MEMOIZABLE.to_string()))
                } else if hook_ask.is_some() {
                    // No key -> the memory is neither read nor written: a hook keeps
                    // its say on every call of the session.
                    (None, Some(HOOK_NOT_MEMOIZABLE.to_string()))
                } else {
                    match tool.approval_memo(&call.input) {
                        ApprovalMemo::Key(tokens) => (
                            Some(ApprovalKey::new(&call.name, &tokens, &self.ctx.workspace)),
                            None,
                        ),
                        ApprovalMemo::Refused(reason) => (None, Some(reason.to_string())),
                        ApprovalMemo::NotApplicable => (None, None),
                    }
                };
                match key.as_ref().and_then(|k| self.approvals.lookup(k)) {
                    // Remembered allow: no question, the user already answered
                    // for this exact token sequence in this directory.
                    Some(true) => {}
                    Some(false) => {
                        return err_outcome(
                            id,
                            format!(
                                "action \"{}\" refused for this session by the user (remembered answer)",
                                call.name
                            ),
                            ToolErrorKind::PermissionDenied,
                        );
                    }
                    None => {
                        let req = PermissionRequest {
                            call_id: id.clone(),
                            tool: call.name.clone(),
                            reason: ask_reason(taint_forced, hook_ask.as_deref()),
                            taint_forced,
                            mode: format!("{mode:?}"),
                            input_summary: summarize(&call.input),
                            input: call.input.clone(),
                            memoizable: key.is_some(),
                            memo_refused,
                        };
                        events.emit(ToolDispatchEvent::PermissionAsk(PermissionReq {
                            call_id: id.clone(),
                            tool: req.tool.clone(),
                            reason: req.reason.clone(),
                            taint_forced: req.taint_forced,
                            input_summary: req.input_summary.clone(),
                            input: req.input.clone(),
                            mode: req.mode.clone(),
                        }));
                        let answer = self.approver.approve(&req).await;
                        if answer.remember
                            && let Some(key) = key
                        {
                            self.approvals.remember(key, answer.allow);
                        }
                        if !answer.allow {
                            return err_outcome(
                                id,
                                format!("action \"{}\" rejected by user", call.name),
                                ToolErrorKind::PermissionDenied,
                            );
                        }
                    }
                }
            }
            Resolved::Allow => {}
        }

        // 4. call() under timeout (a tool that hangs does not block the loop).
        // The context of THIS call carries its output emitter (US-015): the
        // fragments travel on the same channel as permission requests, already
        // correlated by `id`.
        // The input is kept only when a later hook is watching this tool: without
        // a declaration, nothing is cloned (US-017 AC5).
        let post_input = self
            .hooks
            .intercepts(HookEvent::PostToolUse, &call.name)
            .then(|| call.input.clone());
        let ctx = self.ctx.with_output_sink({
            let events = events.clone();
            let id = id.clone();
            std::sync::Arc::new(move |chunk: String| {
                events.emit(ToolDispatchEvent::OutputDelta {
                    id: id.clone(),
                    chunk,
                });
            })
        });
        let untrusted = tool.returns_untrusted();
        let outcome =
            match tokio::time::timeout(tool.timeout(&ctx), tool.invoke(call.input, &ctx)).await {
                Err(_elapsed) => {
                    if untrusted {
                        self.taint.mark();
                    }
                    err_outcome_tainted(
                        id,
                        ToolError::Timeout.to_string(),
                        untrusted,
                        ToolErrorKind::Timeout,
                    )
                }
                Ok(Err(e)) => {
                    if untrusted {
                        self.taint.mark();
                    }
                    err_outcome_tainted(id, e.to_string(), untrusted, e.kind())
                }
                Ok(Ok(out)) => {
                    // 5. taint: an untrusted output just entered the context.
                    if untrusted {
                        self.taint.mark();
                    }
                    ToolOutcome {
                        id,
                        content: out.content,
                        is_error: out.is_error,
                        untrusted,
                        error_kind: out.is_error.then_some(ToolErrorKind::Semantic),
                    }
                }
            };

        // 6. hooks PostToolUse (4.3): only after a call that actually ran. The
        // outcome is already decided and is passed by reference: a later hook
        // observes, it does not rewrite (US-019 AC4).
        if let Some(input) = post_input {
            self.hooks
                .post_tool_use(
                    &call.name,
                    &input,
                    HookToolResult {
                        content: &outcome.content,
                        is_error: outcome.is_error,
                    },
                )
                .await;
        }
        outcome
    }

    fn can_run_parallel_without_permission(&self, call: &ToolInvocation) -> bool {
        let Some(tool) = self.tools.get(&call.name) else {
            return false;
        };
        if !(tool.is_concurrency_safe() && tool.is_read_only() && !tool.is_taint_sensitive()) {
            return false;
        }
        // A watched call leaves the parallel segment: a hook may turn it into a
        // question, and two confirmations must never overlap.
        if self.hooks.intercepts(HookEvent::PreToolUse, &call.name) {
            return false;
        }
        if tool.precheck(&call.input, &self.ctx).is_err() {
            return true;
        }
        let mode = self.mode();
        let pctx = PermCtx {
            mode,
            taint_recent: self.taint.is_recent(),
        };
        let resolved = resolve_permission(
            mode,
            tool.permission(&call.input, &pctx),
            tool.is_read_only(),
            tool.is_sensitive(),
            tool.is_taint_sensitive(),
            pctx.taint_recent,
        );
        !matches!(resolved, Resolved::Ask)
    }

    async fn run_parallel_segment(
        &self,
        segment: Vec<(usize, ToolInvocation)>,
        events: ToolEventSink,
    ) -> Vec<(usize, ToolOutcome)> {
        stream::iter(segment)
            .map(|(i, call)| {
                let events = events.clone();
                async move { (i, self.run_one(call, events).await) }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await
    }
}

#[async_trait]
impl ToolDispatch for Registry {
    fn seed_taint(&self, recent_untrusted: bool) {
        Registry::seed_taint(self, recent_untrusted);
    }

    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ToolOutcome> {
        let started_tainted = self.taint.is_recent();
        // New dispatch cycle: shrinks the taint window.
        self.taint.begin_cycle();

        let mut indexed: Vec<(usize, ToolOutcome)> = Vec::new();
        let mut segment: Vec<(usize, ToolInvocation)> = Vec::new();

        for (i, call) in calls.into_iter().enumerate() {
            if self.can_run_parallel_without_permission(&call) {
                segment.push((i, call));
                continue;
            }
            if !segment.is_empty() {
                indexed.extend(
                    self.run_parallel_segment(std::mem::take(&mut segment), events.clone())
                        .await,
                );
            }
            indexed.push((i, self.run_one(call, events.clone()).await));
        }
        if !segment.is_empty() {
            indexed.extend(self.run_parallel_segment(segment, events.clone()).await);
        }

        // Restores the batch order (deterministic transcripts/tests).
        indexed.sort_by_key(|(i, _)| *i);
        let outcomes: Vec<ToolOutcome> = indexed.into_iter().map(|(_, o)| o).collect();
        if started_tainted
            && !outcomes.is_empty()
            && outcomes.iter().all(|o| o.is_error && !o.untrusted)
        {
            self.taint.mark();
        }
        outcomes
    }
}

fn err_outcome(
    id: agent_core::message::ToolCallId,
    msg: String,
    error_kind: ToolErrorKind,
) -> ToolOutcome {
    // Pipeline error (refusal/unknown/parse): in-house content, not tainted.
    ToolOutcome {
        id,
        content: msg,
        is_error: true,
        untrusted: false,
        error_kind: Some(error_kind),
    }
}

fn err_outcome_tainted(
    id: agent_core::message::ToolCallId,
    msg: String,
    untrusted: bool,
    error_kind: ToolErrorKind,
) -> ToolOutcome {
    ToolOutcome {
        id,
        content: msg,
        is_error: true,
        untrusted,
        error_kind: Some(error_kind),
    }
}

fn ask_reason(taint_forced: bool, hook: Option<&str>) -> String {
    if taint_forced {
        "sensitive action derived from untrusted content (injection defense)".to_string()
    } else if let Some(reason) = hook {
        format!("confirmation required by a hook: {reason}")
    } else {
        "sensitive action requires confirmation".to_string()
    }
}

/// Short summary of a tool input for the confirmation prompt.
fn summarize(input: &serde_json::Value) -> String {
    let s = input.to_string();
    if s.len() > 200 {
        format!("{}…", truncate_utf8_prefix(&s, 200))
    } else {
        s
    }
}

fn truncate_utf8_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `Registry` builder. The default `Approver` is `AutoDeny` (fail-closed:
/// without an explicit counterpart, every confirmation fails).
pub struct RegistryBuilder {
    tools: HashMap<String, Box<dyn DynTool>>,
    mode: PermissionModeState,
    approver: Option<Arc<dyn Approver>>,
    approvals: ApprovalMemory,
    taint_window: u64,
    initial_taint_recent: bool,
    hooks: Option<Arc<dyn Hooks>>,
    ctx: ToolCtx,
}

impl RegistryBuilder {
    pub fn mode(self, mode: PermissionMode) -> Self {
        self.mode.set(mode);
        self
    }
    pub fn mode_state(mut self, mode: PermissionModeState) -> Self {
        self.mode = mode;
        self
    }
    pub fn approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }
    /// Session approval memory shared with the frontend (`/approvals`), like
    /// the permission mode. Defaults to a memory owned by the registry alone.
    pub fn approvals(mut self, approvals: ApprovalMemory) -> Self {
        self.approvals = approvals;
        self
    }
    pub fn taint_window(mut self, window: u64) -> Self {
        self.taint_window = window;
        self
    }
    pub fn initial_taint_recent(mut self, recent: bool) -> Self {
        self.initial_taint_recent = recent;
        self
    }
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.ctx.timeout = timeout;
        self
    }
    /// User hooks run around each tool call (US-017). Default: `NoHooks`, which
    /// costs nothing and lets no one intervene.
    pub fn hooks(mut self, hooks: Arc<dyn Hooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }
    /// Shell command hardening closure (Bash network sandbox), injected
    /// by agent-cli from `agent-sandbox`.
    pub fn command_hardener(mut self, harden: crate::tool::CommandHardener) -> Self {
        self.ctx.harden = Some(harden);
        self
    }
    /// Registers a native tool (boxed into a `DynTool`). An already present name keeps
    /// the first registered tool.
    pub fn register<T: crate::tool::Tool + 'static>(mut self, tool: T) -> Self {
        let dyn_tool = into_dyn(tool);
        self.tools
            .entry(dyn_tool.name().to_string())
            .or_insert(dyn_tool);
        self
    }
    /// Registers an already boxed `DynTool` (e.g. a future MCP tool).
    pub fn register_dyn(mut self, tool: Box<dyn DynTool>) -> Self {
        self.tools.entry(tool.name().to_string()).or_insert(tool);
        self
    }
    pub fn build(self) -> Registry {
        let registry = Registry {
            tools: self.tools,
            mode: self.mode,
            approver: self.approver.unwrap_or_else(|| Arc::new(AutoDeny)),
            approvals: self.approvals,
            taint: TaintTracker::new(self.taint_window),
            hooks: self.hooks.unwrap_or_else(|| Arc::new(NoHooks)),
            ctx: self.ctx,
        };
        registry.seed_taint(self.initial_taint_recent);
        registry
    }
}
