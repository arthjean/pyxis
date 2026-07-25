//! `Registry`: implements `ToolDispatch` (the core <-> tools boundary). Runs
//! each batch in ordered segments: safe reads in parallel, mutations and
//! confirmations serially, then puts every call through the **strict pipeline** (4.3):
//!
//! ```text
//! parse+validate -> permission(mode x taint) -> call() under timeout -> taint -> outcome
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

        // 2. permission: tool baseline shaped by mode + taint (4.4/4.6).
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
                let memo = tool.approval_memo(&call.input);
                let (key, memo_refused) = match (&memo, taint_forced) {
                    (_, true) => (None, Some(TAINT_NOT_MEMOIZABLE.to_string())),
                    (ApprovalMemo::Key(tokens), false) => (
                        Some(ApprovalKey::new(&call.name, tokens, &self.ctx.workspace)),
                        None,
                    ),
                    (ApprovalMemo::Refused(reason), false) => (None, Some((*reason).to_string())),
                    (ApprovalMemo::NotApplicable, false) => (None, None),
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
                            reason: ask_reason(taint_forced),
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

        // 3. call() under timeout (a tool that hangs does not block the loop).
        // The context of THIS call carries its output emitter (US-015): the
        // fragments travel on the same channel as permission requests, already
        // correlated by `id`.
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
                // 4. taint: an untrusted output just entered the context.
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
        }
    }

    fn can_run_parallel_without_permission(&self, call: &ToolInvocation) -> bool {
        let Some(tool) = self.tools.get(&call.name) else {
            return false;
        };
        if !(tool.is_concurrency_safe() && tool.is_read_only() && !tool.is_taint_sensitive()) {
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

fn ask_reason(taint_forced: bool) -> String {
    if taint_forced {
        "sensitive action derived from untrusted content (injection defense)".to_string()
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
            ctx: self.ctx,
        };
        registry.seed_taint(self.initial_taint_recent);
        registry
    }
}
