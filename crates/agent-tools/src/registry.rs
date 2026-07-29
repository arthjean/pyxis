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
use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::event::PermissionReq;
use agent_core::message::ToolErrorKind;
use agent_core::provider::ToolSpec;
use agent_core::tools::{
    ToolDispatch, ToolDispatchEvent, ToolDispatchSnapshot, ToolEventSink, ToolInvocation,
    ToolOutcome, ToolResultStatus,
};
use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::stream::{self, StreamExt};

use crate::error::ToolError;
use crate::hooks::{HookDecision, HookEvent, HookToolResult, Hooks, NoHooks};
use crate::permission::{
    ApprovalKey, ApprovalMemo, ApprovalMemory, Approver, AutoDeny, PermCtx, PermissionMode,
    PermissionModeState, PermissionRequest, Resolved, resolve_permission,
};
use crate::sandbox::{SandboxDenial, SandboxEscalator};
use crate::taint::TaintTracker;
use crate::tool::{DynTool, ToolCtx, into_dyn};

/// Tool description capped at exposure time (a tool does not pollute the prompt).
const MAX_DESCRIPTION: usize = 2048;
/// Reason shown when the taint defense forbids remembering an answer (US-008 AC5).
const TAINT_NOT_MEMOIZABLE: &str = "untrusted content was read in this turn";
/// Reason shown when a hook is what forces the confirmation (US-018 AC2): a
/// remembered answer would silence that hook for the rest of the session.
const HOOK_NOT_MEMOIZABLE: &str = "the confirmation comes from a hook";
/// A sandbox escalation is never rememberable (US-004 AC3/AC4): remembering it
/// would turn a one-call widening into a session-wide policy change, which is
/// exactly what the story forbids.
const ESCALATION_NOT_MEMOIZABLE: &str = "a sandbox escalation covers one call only";
/// Concurrency cap of the read-only batch (ARCHITECTURE 4.2).
const CONCURRENCY: usize = 10;

/// Tool registry + execution policy. Built by the CLI/TUI, injected
/// into the core as `Arc<dyn ToolDispatch>`.
///
/// The exposed set is mutable in session (US-016) but only moves at a **turn
/// boundary**: a change is staged and applied by `commit_staged`, which the
/// frontend calls before composing the specs of the next turn. A server
/// connected while a turn runs therefore never changes the set of tools that turn
/// is dispatching against (AC4).
pub struct Registry {
    tools: std::sync::RwLock<HashMap<String, Arc<dyn DynTool>>>,
    staged: std::sync::Mutex<Vec<StagedChange>>,
    generation: AtomicU64,
    mode: PermissionModeState,
    approver: Arc<dyn Approver>,
    approvals: ApprovalMemory,
    taint: Arc<TaintTracker>,
    hooks: Arc<dyn Hooks>,
    /// Turns an approved sandbox escalation into an actual widening (US-004).
    /// `None` = no perimeter can be widened, hence no escalation is offered.
    escalator: Option<Arc<dyn SandboxEscalator>>,
    ctx: ToolCtx,
}

/// A change to the exposed tool set, waiting for the next turn boundary.
enum StagedChange {
    Add(Vec<Arc<dyn DynTool>>),
    Remove(Vec<String>),
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
            escalator: None,
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

    /// Read access to the exposed set. A poisoned lock is recovered rather than
    /// propagated: losing the tool registry would end the session, which is a
    /// worse outcome than reading a map a panicking thread left consistent
    /// (nothing here is written across an unwind point).
    fn tools_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<dyn DynTool>>> {
        match self.tools.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Stages tools to expose (US-016). They enter the registry at the next
    /// `commit_staged`, never in the middle of a turn. A name already exposed is
    /// kept: a tool the model has already been given must not be replaced under it.
    pub fn stage_tools(&self, tools: Vec<Box<dyn DynTool>>) {
        if tools.is_empty() {
            return;
        }
        self.stage(StagedChange::Add(
            tools.into_iter().map(Arc::from).collect(),
        ));
    }

    /// Stages the removal of exposed tools (server disconnected in session).
    pub fn stage_removal(&self, names: Vec<String>) {
        if names.is_empty() {
            return;
        }
        self.stage(StagedChange::Remove(names));
    }

    fn stage(&self, change: StagedChange) {
        let mut staged = match self.staged.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        staged.push(change);
    }

    /// Applies the staged changes, in the order they were staged. Called at a turn
    /// boundary, before the specs are composed. Returns whether the exposed set
    /// changed, so the frontend can say so.
    pub fn commit_staged(&self) -> bool {
        let pending: Vec<StagedChange> = {
            let mut staged = match self.staged.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if staged.is_empty() {
                return false;
            }
            std::mem::take(&mut staged)
        };
        let mut tools = match self.tools.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut changed = false;
        for change in pending {
            match change {
                StagedChange::Add(added) => {
                    for tool in added {
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            tools.entry(tool.name().to_string())
                        {
                            slot.insert(tool);
                            changed = true;
                        }
                    }
                }
                StagedChange::Remove(names) => {
                    for name in names {
                        changed |= tools.remove(&name).is_some();
                    }
                }
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        changed
    }

    /// Specs exposed to the model (capped descriptions), for `AgentContext.tools`.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        specs_from_tools(&self.tools_read())
    }

    /// Captures specs and an immutable restricted dispatcher under the same
    /// registry read. Later staged changes cannot alter this dispatch view.
    pub fn step_snapshot(&self) -> ToolDispatchSnapshot {
        let tools_guard = self.tools_read();
        let generation = self.generation.load(Ordering::Acquire);
        let tools = tools_guard.clone();
        let specs = specs_from_tools(&tools);
        drop(tools_guard);
        let frozen = Registry {
            tools: std::sync::RwLock::new(tools),
            staged: std::sync::Mutex::new(Vec::new()),
            generation: AtomicU64::new(generation),
            mode: self.mode.clone(),
            approver: Arc::clone(&self.approver),
            approvals: self.approvals.clone(),
            taint: Arc::clone(&self.taint),
            hooks: Arc::clone(&self.hooks),
            escalator: self.escalator.clone(),
            ctx: self.ctx.clone(),
        };
        ToolDispatchSnapshot::new(generation, specs, Arc::new(frozen))
    }

    /// Collects the behavioral guidelines of every tool (US-026), for
    /// injection into the system prompt. Deterministic order (sorted by tool name,
    /// tool guidelines in declaration order) -> stable and cache-friendly
    /// prompt.
    pub fn behavioral_guidelines(&self) -> Vec<String> {
        let tools = self.tools_read();
        let mut names: Vec<&String> = tools.keys().collect();
        names.sort();
        let mut out = Vec::new();
        for n in names {
            if let Some(t) = tools.get(n) {
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
        let started = tokio::time::Instant::now();
        self.run_one_inner(call, events)
            .await
            .with_duration(started.elapsed())
    }

    async fn run_one_inner(&self, call: ToolInvocation, events: ToolEventSink) -> ToolOutcome {
        let id = call.id.clone();
        // The `Arc` is cloned out of the map so no lock is held across an await:
        // a tool removed mid-call keeps running to completion on this handle.
        let Some(tool) = self.tools_read().get(&call.name).cloned() else {
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
        let ctx = self.ctx.for_call(id.clone(), {
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
        // The retry of an escalation (US-004) needs the input again. Cloned only
        // when a perimeter can actually be widened: without an escalator, no
        // offer is possible and nothing is copied.
        let retry_input = self.escalator.as_ref().map(|_| call.input.clone());
        let mut denial: Option<SandboxDenial> = None;
        let outcome = match tokio::time::timeout(
            tool.timeout(&ctx),
            std::panic::AssertUnwindSafe(tool.invoke(call.input, &ctx)).catch_unwind(),
        )
        .await
        {
            Err(_elapsed) => {
                if untrusted {
                    self.taint.mark();
                }
                err_outcome_tainted(
                    id.clone(),
                    ToolError::Timeout.to_string(),
                    untrusted,
                    ToolErrorKind::Timeout,
                )
            }
            Ok(Err(_panic)) => {
                if untrusted {
                    self.taint.mark();
                }
                err_outcome_tainted(
                    id.clone(),
                    "tool adapter panicked before producing a result".to_string(),
                    untrusted,
                    ToolErrorKind::Io,
                )
            }
            Ok(Ok(Err(e))) => {
                if untrusted {
                    self.taint.mark();
                }
                err_outcome_tainted(id.clone(), e.to_string(), untrusted, e.kind())
            }
            Ok(Ok(Ok(out))) => {
                // 5. taint: an untrusted output just entered the context.
                if untrusted {
                    self.taint.mark();
                }
                let sandbox_denied = out.denial.is_some();
                denial = out.denial;
                // US-009: the plan is addressed to the client, so it travels
                // on the event channel, correlated to nothing but this turn.
                if let Some(plan) = out.plan {
                    events.emit(ToolDispatchEvent::Plan(plan));
                }
                outcome_from(
                    id.clone(),
                    out.content,
                    out.structured_content,
                    out.is_error,
                    untrusted,
                    out.images,
                    out.execution,
                    sandbox_denied,
                )
            }
        };

        // 5bis. US-004: a failure the sandbox caused, and that a perimeter can
        // actually lift, becomes an explicit offer to re-run this ONE call.
        let outcome = match (denial, retry_input) {
            (Some(denial), Some(input))
                if self
                    .escalator
                    .as_ref()
                    .is_some_and(|escalator| escalator.can_lift(&denial)) =>
            {
                self.escalate(tool.as_ref(), &denial, input, outcome, &ctx, &events)
                    .await
            }
            _ => outcome,
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

    /// Offers, then applies, a one-call widening of the confinement (US-004).
    ///
    /// Deliberately OUTSIDE `resolve_permission`: the question is asked directly,
    /// so no permission mode short-circuits it and the escalation is never
    /// automatic, not even under `BypassPermissions` (AC4). It is never
    /// rememberable either, and the widening lives exactly as long as the guard
    /// (AC3). A refusal, an absent escalator, or a cause the escalator cannot
    /// lift all keep the original failure: fail-closed on the doubt (AC6).
    async fn escalate(
        &self,
        tool: &dyn DynTool,
        denial: &SandboxDenial,
        input: serde_json::Value,
        original: ToolOutcome,
        ctx: &ToolCtx,
        events: &ToolEventSink,
    ) -> ToolOutcome {
        let untrusted = tool.returns_untrusted();
        let Some(escalator) = self.escalator.as_ref() else {
            return original;
        };
        let id = original.id.clone();
        let mode = self.mode();
        // AC5: recent taint forces the confirmation whatever the mode. Here it
        // also denies any automatic approver, which is the fail-closed side of
        // the same rule.
        let taint_forced = self.taint.is_recent() && tool.is_taint_sensitive();
        let req = PermissionRequest {
            call_id: id.clone(),
            tool: tool.name().to_string(),
            reason: denial.escalation_reason(),
            taint_forced,
            mode: format!("{mode:?}"),
            input_summary: summarize(&input),
            input: input.clone(),
            memoizable: false,
            memo_refused: Some(ESCALATION_NOT_MEMOIZABLE.to_string()),
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
        if !self.approver.approve(&req).await.allow {
            return original;
        }
        // The guard IS the widening: it lives on the stack for the duration of
        // the retry and revokes on drop, so no path can leak it into the session.
        let Some(_grant) = escalator.lift(denial) else {
            return original;
        };
        tracing::debug!(
            target: "pyxis::tools",
            tool = %req.tool,
            "sandbox escalation granted for one call"
        );
        match tokio::time::timeout(
            tool.timeout(ctx),
            std::panic::AssertUnwindSafe(tool.invoke(input, ctx)).catch_unwind(),
        )
        .await
        {
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
            Ok(Err(_panic)) => {
                if untrusted {
                    self.taint.mark();
                }
                err_outcome_tainted(
                    id,
                    "tool adapter panicked before producing a result".to_string(),
                    untrusted,
                    ToolErrorKind::Io,
                )
            }
            Ok(Ok(Err(e))) => {
                if untrusted {
                    self.taint.mark();
                }
                err_outcome_tainted(id, e.to_string(), untrusted, e.kind())
            }
            Ok(Ok(Ok(out))) => {
                if untrusted {
                    self.taint.mark();
                }
                let sandbox_denied = out.denial.is_some();
                if let Some(plan) = out.plan {
                    events.emit(ToolDispatchEvent::Plan(plan));
                }
                outcome_from(
                    id,
                    out.content,
                    out.structured_content,
                    out.is_error,
                    untrusted,
                    out.images,
                    out.execution,
                    sandbox_denied,
                )
            }
        }
    }

    fn can_run_parallel_without_permission(&self, call: &ToolInvocation) -> bool {
        let Some(tool) = self.tools_read().get(&call.name).cloned() else {
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
    ToolOutcome::new(id, msg, true, false, Some(error_kind))
}

/// Outcome of a call that ran to completion, error or not.
#[allow(clippy::too_many_arguments)]
fn outcome_from(
    id: agent_core::message::ToolCallId,
    content: String,
    structured_content: Option<serde_json::Value>,
    is_error: bool,
    untrusted: bool,
    images: Vec<agent_core::tools::ToolImage>,
    execution: Option<agent_core::tools::ToolExecution>,
    sandbox_denied: bool,
) -> ToolOutcome {
    let mut outcome = ToolOutcome {
        structured_content,
        execution,
        images,
        ..ToolOutcome::new(
            id,
            content,
            is_error,
            untrusted,
            is_error.then_some(ToolErrorKind::Semantic),
        )
    };
    if sandbox_denied {
        outcome.status = ToolResultStatus::SandboxDenied;
        outcome.is_error = true;
        outcome.error_kind = Some(ToolErrorKind::SandboxDenied);
    } else if outcome
        .execution
        .as_ref()
        .is_some_and(|execution| execution.timed_out)
    {
        outcome.status = ToolResultStatus::TimedOut;
        outcome.is_error = true;
        outcome.error_kind = Some(ToolErrorKind::Timeout);
    }
    outcome
}

fn err_outcome_tainted(
    id: agent_core::message::ToolCallId,
    msg: String,
    untrusted: bool,
    error_kind: ToolErrorKind,
) -> ToolOutcome {
    ToolOutcome::new(id, msg, true, untrusted, Some(error_kind))
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

fn specs_from_tools(tools: &HashMap<String, Arc<dyn DynTool>>) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = tools
        .values()
        .map(|tool| {
            let raw_description = tool.description();
            ToolSpec {
                name: tool.name().to_string(),
                description: truncate_utf8_prefix(&raw_description, MAX_DESCRIPTION).to_string(),
                kind: tool.kind(),
            }
        })
        .collect();
    specs.sort_by(|left, right| left.name.cmp(&right.name));
    specs
}

/// `Registry` builder. The default `Approver` is `AutoDeny` (fail-closed:
/// without an explicit counterpart, every confirmation fails).
pub struct RegistryBuilder {
    tools: HashMap<String, Arc<dyn DynTool>>,
    mode: PermissionModeState,
    approver: Option<Arc<dyn Approver>>,
    approvals: ApprovalMemory,
    taint_window: u64,
    initial_taint_recent: bool,
    hooks: Option<Arc<dyn Hooks>>,
    escalator: Option<Arc<dyn SandboxEscalator>>,
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
    /// Does the active model read images (US-011)? Read from the provider
    /// capabilities by agent-cli.
    pub fn vision(mut self, vision: bool) -> Self {
        self.ctx.vision = vision;
        self
    }
    /// Persistent shell sessions of the run (US-012), owned by agent-cli so the
    /// end of the run can close them.
    pub fn exec_sessions(mut self, sessions: crate::exec_session::ExecSessions) -> Self {
        self.ctx.sessions = sessions;
        self
    }
    /// Confinement perimeter in force for the session (US-001), and whether the
    /// kernel really carries it (US-004 classification).
    pub fn sandbox(mut self, policy: agent_core::sandbox::SandboxPolicy, enforced: bool) -> Self {
        self.ctx.sandbox = policy;
        self.ctx.sandbox_enforced = enforced;
        self
    }
    /// What the confinement blocked during a call (US-004), injected by
    /// agent-cli over the network proxy.
    pub fn sandbox_observer(mut self, observer: Arc<dyn crate::sandbox::SandboxObserver>) -> Self {
        self.ctx.sandbox_observer = Some(observer);
        self
    }
    /// Applies an approved one-call widening (US-004). Without it no escalation
    /// is ever offered: a perimeter nobody can widen must not be advertised.
    pub fn sandbox_escalator(mut self, escalator: Arc<dyn SandboxEscalator>) -> Self {
        self.escalator = Some(escalator);
        self
    }
    /// Registers a native tool (boxed into a `DynTool`). An already present name keeps
    /// the first registered tool.
    pub fn register<T: crate::tool::Tool + 'static>(mut self, tool: T) -> Self {
        let dyn_tool: Arc<dyn DynTool> = Arc::from(into_dyn(tool));
        self.tools
            .entry(dyn_tool.name().to_string())
            .or_insert(dyn_tool);
        self
    }
    /// Registers an already boxed `DynTool` (e.g. an MCP tool).
    pub fn register_dyn(mut self, tool: Box<dyn DynTool>) -> Self {
        let tool: Arc<dyn DynTool> = Arc::from(tool);
        self.tools.entry(tool.name().to_string()).or_insert(tool);
        self
    }
    pub fn build(self) -> Registry {
        let registry = Registry {
            tools: std::sync::RwLock::new(self.tools),
            staged: std::sync::Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
            mode: self.mode,
            approver: self.approver.unwrap_or_else(|| Arc::new(AutoDeny)),
            approvals: self.approvals,
            taint: Arc::new(TaintTracker::new(self.taint_window)),
            hooks: self.hooks.unwrap_or_else(|| Arc::new(NoHooks)),
            escalator: self.escalator,
            ctx: self.ctx,
        };
        registry.seed_taint(self.initial_taint_recent);
        registry
    }
}
