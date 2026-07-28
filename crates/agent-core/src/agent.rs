//! `run_agent`: the agent loop, a state machine with typed transitions, exposed
//! as a `Stream<AgentEvent>` (async-stream). Headless: it pushes nothing to a
//! terminal, it yields structured events (never ANSI).
//!
//! Implements: transcript-before-response (invariant 6), withholding
//! (context PendingError, invariant 8), cascading compaction (section 5),
//! cross-cutting retry of transient errors (≠ withholding), and the exhaustive
//! `match` on `Transition` (AC1).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};

use crate::budget::{ContextBudget, estimate_input, estimate_static_input};
use crate::cancel::{Cancellable, guard};
use crate::compaction::{CompactKind, CompactionState, full_compact, microcompact};
use crate::deps::Deps;
use crate::error::{AgentError, ProviderFailure};
use crate::event::{AgentEvent, ToolCallView, ToolOutputDeltaView, ToolResultView};
use crate::guardrail::{CostBudget, LoopDecision, LoopGuard, UsageBudget, batch_signature};
use crate::input::InputQueue;
use crate::message::{INTERRUPTED_TOOL_RESULT, Message, ToolCallId, unanswered_tool_calls};
use crate::model::{ResolvedModelRuntime, TruncationMode};
use crate::provider::{
    AuthError, CanonicalRequest, ErrorClass, ProviderError, StreamEvent, TokenUsage, ToolSpec,
};
use crate::step::StepContextSource;
use crate::tools::{
    MAX_MODEL_TOOL_RESULT_BYTES, ModelToolResult, StepToolPlan, ToolDispatchEvent,
    ToolDispatchSnapshot, ToolEventSink, ToolOutcome,
};
use crate::transition::{
    Accumulator, ContextErrorKind, ExhaustReason, PendingError, Transition, post_stream_transition,
    pre_stream_transition,
};

/// Loop settings (guardrails, thresholds).
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub max_turns: u32,
    pub max_output_tokens: u32,
    pub max_retries: u32,
    pub micro_keep_recent: usize,
    pub compaction_breaker_limit: u32,
    pub backoff_base_ms: u64,
    /// US-014: identical tool-batch repeats before the loop signal
    /// (default 3). Past the signal -> deterministic stop.
    pub loop_guard_threshold: u32,
    /// US-014: cumulated token budget (kill-switch). `None` = disabled.
    pub token_budget: Option<u64>,
    /// US-014: cumulated cost budget (kill-switch). `None` = disabled.
    pub cost_budget: Option<CostBudget>,
    /// Optional fallback model after a provider overload.
    pub overload_fallback_model: Option<String>,
    /// Calibration probe (US-002): when enabled, every model round-trip carries
    /// the local estimate of its input next to the backend measure, so a client
    /// can compare them. Off by default: the estimate costs a tokenizer pass
    /// over the whole transcript.
    pub usage_probe: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_output_tokens: 4096,
            max_retries: 3,
            micro_keep_recent: 2,
            compaction_breaker_limit: 3,
            backoff_base_ms: 50,
            loop_guard_threshold: 3,
            token_budget: None,
            cost_budget: None,
            overload_fallback_model: None,
            usage_probe: false,
        }
    }
}

/// Context of an agent run (model, system, transcript, tools).
pub struct AgentContext {
    pub model: String,
    /// Immutable model contract captured before the turn starts.
    pub model_runtime: Option<ResolvedModelRuntime>,
    pub reasoning_effort: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub config: RunConfig,
    /// EPHEMERAL context messages (US-028): AGENTS.md + environment block,
    /// prefixed to EVERY request but NEVER pushed into `messages` nor persisted
    /// (reloaded per turn, not accumulated). Stateless-safe: project context is
    /// re-supplied every turn without polluting the transcript or `instructions`.
    pub context_messages: Vec<Message>,
    /// Ephemeral control messages appended after the transcript for the current
    /// request, without persistence. Example: automatic goal re-prompt.
    pub ephemeral_messages: Vec<Message>,
    /// US-006: rebuilds `tools` and `context_messages` before EVERY model
    /// request. `None` keeps the values above frozen for the whole run, which is
    /// the historical behavior.
    pub step_source: Option<Arc<dyn StepContextSource>>,
    /// US-007: inputs accepted for THIS turn while it runs. Drained at a safe
    /// point, never mid-stream. `None` disables steering.
    pub inputs: Option<Arc<dyn InputQueue>>,
}

impl AgentContext {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            model_runtime: None,
            reasoning_effort: None,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            config: RunConfig::default(),
            context_messages: Vec::new(),
            ephemeral_messages: Vec::new(),
            step_source: None,
            inputs: None,
        }
    }
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }
    pub fn push(mut self, msg: Message) -> Self {
        self.messages.push(msg);
        self
    }
    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = config;
        self
    }
    pub fn with_context_messages(mut self, messages: Vec<Message>) -> Self {
        self.context_messages = messages;
        self
    }
    pub fn with_ephemeral_messages(mut self, messages: Vec<Message>) -> Self {
        self.ephemeral_messages = messages;
        self
    }
    pub fn with_step_source(mut self, source: Arc<dyn StepContextSource>) -> Self {
        self.step_source = Some(source);
        self
    }
    pub fn with_inputs(mut self, inputs: Arc<dyn InputQueue>) -> Self {
        self.inputs = Some(inputs);
        self
    }
}

/// Waits for a steering input, or forever when the run has no input queue.
///
/// A dedicated future rather than an `Option` arm in the `select!`: an absent
/// queue must stay pending, never resolve, otherwise the sampling loop would spin.
async fn steer_ready(inputs: &Option<Arc<dyn InputQueue>>) {
    match inputs {
        Some(queue) => queue.ready().await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_request(
    model: &str,
    model_runtime: &Option<ResolvedModelRuntime>,
    reasoning_effort: &Option<String>,
    system: &Option<String>,
    context_messages: &[Message],
    messages: &[Message],
    ephemeral_messages: &[Message],
    tools: &[ToolSpec],
    max_output: u32,
) -> CanonicalRequest {
    // US-028: EPHEMERAL prefix (AGENTS.md + env). Stable before volatile to
    // preserve the cacheable prefix; never persisted (the transcript stays
    // `messages` alone).
    let mut all =
        Vec::with_capacity(context_messages.len() + messages.len() + ephemeral_messages.len());
    all.extend_from_slice(context_messages);
    all.extend_from_slice(messages);
    all.extend_from_slice(ephemeral_messages);
    CanonicalRequest {
        model: model.to_string(),
        model_runtime: model_runtime.clone(),
        reasoning_effort: reasoning_effort.clone(),
        system: system.clone(),
        messages: all,
        tools: tools.to_vec(),
        max_output_tokens: max_output,
    }
}

fn backoff(config: &RunConfig, attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(5);
    Duration::from_millis(config.backoff_base_ms.saturating_mul(factor))
}

/// Cap on the honored `Retry-After` delay (US-023). A server cannot freeze the
/// loop forever: an absurd delay is bounded, we retry then give up according
/// to `max_retries`. Same cap as Pi (60 s).
const MAX_RETRY_AFTER_MS: u64 = 60_000;

/// Effective retry delay (US-023): `max(exponential backoff, Retry-After)`, the
/// server delay (exact ms) winning when it is longer, bounded by
/// `MAX_RETRY_AFTER_MS`. Errors without a server header fall back on the backoff.
fn retry_delay(base: Duration, err: &ProviderError) -> Duration {
    match err {
        ProviderError::Http {
            retry_after_ms: Some(ms),
            ..
        } => base.max(Duration::from_millis((*ms).min(MAX_RETRY_AFTER_MS))),
        _ => base,
    }
}

fn retry_jitter_ms(
    now_ms: u64,
    attempt: u32,
    class: ErrorClass,
    err: &ProviderError,
    cap_ms: u64,
) -> u64 {
    if cap_ms == 0 {
        return 0;
    }
    let class_code = match class {
        ErrorClass::Retryable => 1,
        ErrorClass::RateLimited => 2,
        ErrorClass::Overloaded(status) => status as u64,
        ErrorClass::Auth(_) => 3,
        ErrorClass::InvalidRequest => 4,
    };
    let status_code = match err {
        ProviderError::Http { status, .. } => *status as u64,
        ProviderError::Transport(_) => 10,
        ProviderError::Decode(_) => 11,
        ProviderError::Stream(_) => 12,
        ProviderError::ContextLengthExceeded => 13,
    };
    let mut x = now_ms
        ^ ((attempt as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        ^ (class_code << 32)
        ^ status_code;
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    1 + (x % cap_ms)
}

fn transient_retry_delay(
    config: &RunConfig,
    attempt: u32,
    class: ErrorClass,
    err: &ProviderError,
    now_ms: u64,
) -> Duration {
    let mut base = backoff(config, attempt);
    if matches!(class, ErrorClass::Overloaded(_)) {
        base = base.saturating_mul(3);
    }
    let delay = retry_delay(base, err);
    if matches!(
        err,
        ProviderError::Http {
            retry_after_ms: Some(ms),
            ..
        } if *ms >= MAX_RETRY_AFTER_MS
    ) {
        return delay;
    }
    let delay_ms = delay.as_millis().min(u64::MAX as u128) as u64;
    let jitter_cap = (delay_ms / 5).min(250);
    delay.saturating_add(Duration::from_millis(retry_jitter_ms(
        now_ms, attempt, class, err, jitter_cap,
    )))
}

fn maybe_switch_to_overload_fallback(
    model: &mut String,
    config: &RunConfig,
    fallback_used: &mut bool,
    class: ErrorClass,
) -> bool {
    if !matches!(class, ErrorClass::Overloaded(_)) || *fallback_used {
        return false;
    }
    let Some(fallback) = config
        .overload_fallback_model
        .as_deref()
        .map(str::trim)
        .filter(|fallback| !fallback.is_empty() && *fallback != model)
    else {
        return false;
    };
    *model = fallback.to_string();
    *fallback_used = true;
    true
}

fn validate_tool_outcomes(
    expected_ids: &[ToolCallId],
    outcomes: &[ToolOutcome],
) -> Result<(), AgentError> {
    use std::collections::HashSet;

    if outcomes.len() != expected_ids.len() {
        return Err(AgentError::Provider(ProviderFailure::contract(format!(
            "tool dispatcher returned {} outcomes for {} calls",
            outcomes.len(),
            expected_ids.len()
        ))));
    }
    let expected: HashSet<&str> = expected_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for outcome in outcomes {
        if !expected.contains(outcome.id.as_str()) {
            return Err(AgentError::Provider(ProviderFailure::contract(format!(
                "tool dispatcher returned unknown call id: {}",
                outcome.id
            ))));
        }
        if !seen.insert(outcome.id.as_str()) {
            return Err(AgentError::Provider(ProviderFailure::contract(format!(
                "tool dispatcher returned duplicate call id: {}",
                outcome.id
            ))));
        }
    }
    if let Some(missing) = expected_ids.iter().find(|id| !seen.contains(id.as_str())) {
        return Err(AgentError::Provider(ProviderFailure::contract(format!(
            "tool dispatcher omitted call id: {missing}"
        ))));
    }
    Ok(())
}

/// US-002: writes a synthetic result for every tool call left unanswered at
/// interruption time, and returns the views to emit to clients.
///
/// Called BEFORE any persistence: a transcript carrying a `tool_use` without a
/// `tool_result` is rejected with a 400 by the backend on the next turn, which
/// makes the session unusable. Real results already collected are kept as they
/// are: reconciliation replaces nothing, it completes.
fn reconcile_interrupted_calls(messages: &mut Vec<Message>) -> Vec<ToolResultView> {
    let pending = unanswered_tool_calls(messages);
    let mut views = Vec::with_capacity(pending.len());
    for id in pending {
        let result = ModelToolResult::cancelled(id, INTERRUPTED_TOOL_RESULT);
        views.push(ToolResultView::from_model(&result));
        messages.push(Message::tool_result_from_model(&result));
    }
    views
}

fn feedback_limits(model_runtime: &Option<ResolvedModelRuntime>) -> (usize, usize) {
    match model_runtime.as_ref().map(|runtime| runtime.truncation) {
        Some(policy) if policy.mode == TruncationMode::Tokens => (
            usize::try_from(policy.limit).unwrap_or(usize::MAX),
            MAX_MODEL_TOOL_RESULT_BYTES,
        ),
        Some(policy) => (
            usize::MAX,
            usize::try_from(policy.limit)
                .unwrap_or(MAX_MODEL_TOOL_RESULT_BYTES)
                .min(MAX_MODEL_TOOL_RESULT_BYTES),
        ),
        None => (usize::MAX, MAX_MODEL_TOOL_RESULT_BYTES),
    }
}

fn estimate_current_input(messages: &[Message], static_input_tokens: u32, deps: &Deps) -> u32 {
    estimate_input(messages, deps.tokenizer.as_ref()).saturating_add(static_input_tokens)
}

fn record_attempt_usage(
    usage_budget: &mut UsageBudget,
    budget: &mut ContextBudget,
    last_usage: Option<TokenUsage>,
    messages: &[Message],
    static_input_tokens: u32,
    acc: &Accumulator,
    deps: &Deps,
) {
    if let Some(u) = last_usage {
        usage_budget.record_usage(u);
    } else {
        let est_in = estimate_current_input(messages, static_input_tokens, deps);
        let est_out = acc.estimate_output(deps.tokenizer.as_ref());
        budget.observe_estimated(est_in);
        usage_budget.record(est_in as u64, est_out as u64);
    }
}

fn rebuild_budget_after_model_switch(
    model: &str,
    config: &RunConfig,
    messages: &[Message],
    static_input_tokens: u32,
    deps: &Deps,
) -> Result<ContextBudget, String> {
    let mut budget = ContextBudget::try_for_model(
        deps.provider.max_context_for_model(model),
        config.max_output_tokens,
    )?;
    budget.observe_estimated(estimate_current_input(messages, static_input_tokens, deps));
    Ok(budget)
}

/// Starts the agent. Returns a `Stream<AgentEvent>` to consume (TUI, `-p`).
pub fn run_agent(ctx: AgentContext, deps: Deps) -> impl Stream<Item = AgentEvent> + Send {
    async_stream::stream! {
        let AgentContext {
            mut model,
            model_runtime,
            mut reasoning_effort,
            system,
            mut messages,
            tools,
            mut config,
            mut context_messages,
            ephemeral_messages,
            step_source,
            inputs,
        } = ctx;

        if let Some(runtime) = &model_runtime {
            if let Err(error) = runtime.validate() {
                yield AgentEvent::Error(AgentError::InvalidRequest(error.to_string()));
                return;
            }
            model = runtime.slug.clone();
            reasoning_effort = runtime.reasoning_effort.clone();
            config.max_output_tokens = runtime.max_output_tokens;
            config.max_retries = runtime.retry.max_retries;
            config.backoff_base_ms = runtime.retry.backoff_base_ms;
            config.overload_fallback_model = None;
        }

        let parallel_tools = model_runtime
            .as_ref()
            .is_none_or(|runtime| runtime.supports_parallel_tool_calls);
        let mut tool_plan = StepToolPlan::capture(
            ToolDispatchSnapshot::new(0, tools, Arc::clone(&deps.tools)),
            parallel_tools,
        );

        // ContextBudget computed for the active model (recomputed on overload fallback).
        let max_context = model_runtime
            .as_ref()
            .map(|runtime| runtime.context_window)
            .unwrap_or_else(|| deps.provider.max_context_for_model(&model));
        let auto_compact_token_limit = model_runtime
            .as_ref()
            .map(|runtime| runtime.auto_compact_token_limit);
        let mut budget = match ContextBudget::try_for_model_with_auto_limit(
            max_context,
            config.max_output_tokens,
            auto_compact_token_limit,
        ) {
            Ok(budget) => budget,
            Err(e) => {
                yield AgentEvent::Error(AgentError::InvalidRequest(e));
                return;
            }
        };
        // Backend usage counts everything that is sent: system, ephemeral
        // context, tool schemas and transcript. Local projections must carry
        // the same static overhead, otherwise compaction comes too late.
        let mut static_input_tokens = estimate_static_input(
            &system,
            &context_messages,
            tool_plan.specs(),
            deps.tokenizer.as_ref(),
        )
        .saturating_add(estimate_input(&ephemeral_messages, deps.tokenizer.as_ref()));
        // US-006: generation of the step frame currently installed. `None` until
        // the first frame, so a source whose first generation is 0 is still read.
        let mut step_generation: Option<u64> = None;
        let mut compaction = CompactionState::default();
        let mut pending: Option<PendingError> = None;
        let mut model_turns: u32 = 0;
        let mut transient_retries: u32 = 0;
        let mut overload_fallback_used = false;
        let mut iterations: u32 = 0;
        let iter_cap = config.max_turns.saturating_mul(4).saturating_add(32);
        // US-014: deterministic guardrails (override the model's own logic).
        let mut loop_guard = LoopGuard::new(config.loop_guard_threshold);
        let mut usage_budget = UsageBudget::new(config.token_budget, config.cost_budget);
        // US-030 (MidTurn): armed when a long tool_result crosses the threshold ->
        // forces compaction on the next turn, BEFORE calling the model again.
        let mut force_compact = false;

        loop {
            iterations += 1;
            if iterations > iter_cap {
                yield AgentEvent::Error(AgentError::Provider(ProviderFailure::contract(
                    "iteration guard reached",
                )));
                return;
            }

            // transcript-before-response (invariant 6): idempotent delta.
            if let Err(e) = deps.session.sync(&messages).await {
                yield AgentEvent::Error(AgentError::Session(e.to_string()));
                return;
            }

            // US-014: budget kill-switch, cumulated threshold reached -> stop (edge
            // case #3). The PRE-turn estimate happens below, before the stream.
            if let Some(reason) = usage_budget.exceeded() {
                yield AgentEvent::Exhausted(reason);
                return;
            }

            let transition: Transition = if deps.cancel.is_cancelled() {
                // US-001: SINGLE stop boundary, every deep cancellation point loops
                // back here, where the transcript is in a known state.
                Transition::Interrupted
            } else if force_compact && pending.is_none() {
                // US-030 MidTurn: compaction forced by a long tool_result on the
                // previous turn. Withholding (`pending`) stays PRIORITY: if a
                // context error is waiting, we let `pre_stream_transition` handle
                // it (Recover) and the force stays armed for the turn after.
                force_compact = false;
                Transition::Compact(CompactKind::Auto)
            } else {
                match pre_stream_transition(
                pending,
                model_turns,
                config.max_turns,
                budget.should_autocompact(),
            ) {
                Some(t) => {
                    pending = None;
                    t
                }
                None => {
                    // US-007: THE safe point. A steer enters the transcript here,
                    // right before the request is built, never in the middle of a
                    // sampling nor between a `tool_use` and its result. Taking is
                    // what removes it from the queue, so it enters exactly once
                    // and in acceptance order.
                    if let Some(queue) = &inputs {
                        for input in queue.take() {
                            messages.push(input);
                        }
                    }

                    // US-006: the model-visible context is captured per request.
                    // An unchanged generation keeps the previous bytes, which is
                    // what makes two steps without a source change produce the
                    // same cacheable prefix and skip the static re-estimate.
                    if let Some(source) = &step_source {
                        let frame = source.next_frame();
                        if step_generation != Some(frame.generation) {
                            step_generation = Some(frame.generation);
                            let dispatch = match frame.tool_dispatch {
                                Some(snapshot) => snapshot,
                                None if frame.tools.is_empty() => ToolDispatchSnapshot::new(
                                    frame.generation,
                                    Vec::new(),
                                    Arc::clone(&deps.tools),
                                ),
                                None => {
                                    yield AgentEvent::Error(AgentError::InvalidRequest(
                                        "tool-bearing step is missing its frozen dispatch snapshot"
                                            .to_string(),
                                    ));
                                    return;
                                }
                            };
                            tool_plan = StepToolPlan::capture(dispatch, parallel_tools);
                            context_messages = frame.context_messages;
                            static_input_tokens = estimate_static_input(
                                &system,
                                &context_messages,
                                tool_plan.specs(),
                                deps.tokenizer.as_ref(),
                            )
                            .saturating_add(estimate_input(
                                &ephemeral_messages,
                                deps.tokenizer.as_ref(),
                            ));
                            budget.observe_estimated(estimate_current_input(
                                &messages,
                                static_input_tokens,
                                &deps,
                            ));
                        }
                    }

                    // structural (cheap) microcompaction under light pressure.
                    // PURELY IN MEMORY: it truncates the content of old
                    // tool_results (the append-only log keeps the full history;
                    // resume will restore more context, never less). So we do
                    // NOT write a boundary (otherwise the clear-on-boundary
                    // resume would wrongly wipe the transcript).
                    if budget.should_microcompact() {
                        let pruned = microcompact(&mut messages, config.micro_keep_recent);
                        if pruned > 0 {
                            compaction.record_success();
                            budget.observe_estimated(estimate_current_input(&messages, static_input_tokens, &deps));
                            yield AgentEvent::Compacted(CompactKind::Micro);
                        }
                    }

                    // US-014: pre-turn estimate, stop BEFORE a turn whose
                    // projection (estimated context + max output) would cross
                    // the budget (edge case #3, "before a big turn").
                    if usage_budget.is_active() {
                        let est_in = estimate_current_input(&messages, static_input_tokens, &deps) as u64;
                        if let Some(reason) =
                            usage_budget.would_exceed(est_in, config.max_output_tokens as u64)
                        {
                            yield AgentEvent::Exhausted(reason);
                            return;
                        }
                    }

                    budget.begin_turn();
                    let req = make_request(
                        &model,
                        &model_runtime,
                        &reasoning_effort,
                        &system,
                        &context_messages,
                        &messages,
                        &ephemeral_messages,
                        tool_plan.specs(),
                        config.max_output_tokens,
                    );
                    if let Err(e) = req.validate() {
                        yield AgentEvent::Error(AgentError::InvalidRequest(e.to_string()));
                        return;
                    }

                    // US-001: opening the stream can block for several seconds
                    // (TLS + first byte); cancellation takes over without waiting.
                    let opened = match guard(&deps.cancel, deps.provider.stream(req)).await {
                        Cancellable::Cancelled => continue,
                        Cancellable::Completed(opened) => opened,
                    };
                    let mut stream = match opened {
                        Ok(s) => s,
                        Err(e) if e.is_context_error() => {
                            pending = Some(PendingError { kind: ContextErrorKind::PromptTooLong });
                            continue;
                        }
                        Err(e) => {
                            let class = deps.provider.classify_error(&e);
                            match class {
                            ErrorClass::Retryable
                            | ErrorClass::RateLimited
                            | ErrorClass::Overloaded(_) => {
                                if maybe_switch_to_overload_fallback(
                                    &mut model,
                                    &config,
                                    &mut overload_fallback_used,
                                    class,
                                ) {
                                    match rebuild_budget_after_model_switch(
                                        &model,
                                        &config,
                                        &messages,
                                        static_input_tokens,
                                        &deps,
                                    ) {
                                        Ok(next_budget) => budget = next_budget,
                                        Err(e) => {
                                            yield AgentEvent::Error(AgentError::InvalidRequest(e));
                                            return;
                                        }
                                    }
                                    transient_retries = 0;
                                    continue;
                                }
                                if transient_retries >= config.max_retries {
                                    yield AgentEvent::Error((&e).into());
                                    return;
                                }
                                transient_retries += 1;
                                // attempt indexed from 0 -> delays 1x,2x,4x.
                                // US-023: honors Retry-After (max(backoff, retry_after), bounded).
                                // US-001: a backoff can last up to 60 s (bounded
                                // Retry-After); cancellation cuts the wait short, the stop
                                // boundary being the loop head.
                                let _ = guard(
                                    &deps.cancel,
                                    deps.clock.sleep(transient_retry_delay(
                                        &config,
                                        transient_retries - 1,
                                        class,
                                        &e,
                                        deps.clock.now_ms(),
                                    )),
                                )
                                .await;
                                continue;
                            }
                            ErrorClass::Auth(AuthError::Expired) => {
                                if transient_retries >= config.max_retries {
                                    yield AgentEvent::Error(AgentError::Auth(AuthError::Expired));
                                    return;
                                }
                                transient_retries += 1;
                                if let Err(refresh_err) = deps.provider.refresh_auth().await {
                                    yield AgentEvent::Error((&refresh_err).into());
                                    return;
                                }
                                continue;
                            }
                            ErrorClass::Auth(a) => {
                                yield AgentEvent::Error(AgentError::Auth(a));
                                return;
                            }
                            ErrorClass::InvalidRequest => {
                                yield AgentEvent::Error((&e).into());
                                return;
                            }
                        }},
                    };

                    // Stream consumption: live yields (never ANSI).
                    let mut acc = Accumulator::new();
                    let mut stream_err: Option<ProviderError> = None;
                    let mut last_usage: Option<TokenUsage> = None;
                    let mut estimated_input: Option<u32> = None;
                    let mut interrupted = false;
                    let mut steered = false;
                    loop {
                        // US-001: cancellation is polled FIRST (`biased`) -> as soon
                        // as it is signalled, no more `Text` nor `Reasoning` is
                        // emitted, even if the stream has events ready.
                        // US-007: a steer comes SECOND, before the stream itself:
                        // an input accepted while the model is talking must cut
                        // this sampling, not wait for the whole answer to drain.
                        let next = tokio::select! {
                            biased;
                            () = deps.cancel.cancelled() => {
                                interrupted = true;
                                break;
                            }
                            () = steer_ready(&inputs) => {
                                steered = true;
                                break;
                            }
                            ev = stream.next() => ev,
                        };
                        let Some(ev) = next else { break };
                        match ev {
                            Ok(StreamEvent::TextDelta { text }) => {
                                yield AgentEvent::Text(text.clone());
                                if let Err(e) = acc.push(StreamEvent::TextDelta { text }) {
                                    yield AgentEvent::Error(e);
                                    return;
                                }
                            }
                            Ok(StreamEvent::ReasoningDelta { text }) => {
                                yield AgentEvent::Reasoning(text.clone());
                                if let Err(e) = acc.push(StreamEvent::ReasoningDelta { text }) {
                                    yield AgentEvent::Error(e);
                                    return;
                                }
                            }
                            Ok(StreamEvent::Usage { usage }) => {
                                // Calibration probe (US-021 AC3 / US-029): the core
                                // COMPUTES the local estimate when the run asks for
                                // it and carries it in `ModelTurn`; WRITING it is a
                                // client decision (US-002 AC5, invariant 1). Off by
                                // default -> no tokenizer pass on the hot path.
                                if config.usage_probe {
                                    estimated_input = Some(
                                        estimate_input(&messages, deps.tokenizer.as_ref())
                                            .saturating_add(static_input_tokens),
                                    );
                                }
                                budget.observe_usage(usage);
                                last_usage = Some(usage);
                            }
                            Ok(StreamEvent::Quota { snapshot }) => {
                                if !snapshot.is_empty() {
                                    yield AgentEvent::Quota(snapshot);
                                }
                            }
                            Ok(other) => {
                                if let Err(e) = acc.push(other) {
                                    yield AgentEvent::Error(e);
                                    return;
                                }
                            }
                            Err(e) => {
                                stream_err = Some(e);
                                break;
                            }
                        }
                    }

                    if interrupted {
                        // The partial is COMMITTED: `to_assistant_message` only keeps
                        // complete calls (a half-streamed `tool_use` is discarded),
                        // and reconciliation will write them a result when passing
                        // the stop boundary. So what the client saw scroll by stays
                        // in the transcript.
                        if !acc.is_empty() {
                            messages.push(acc.to_assistant_message());
                        }
                        continue;
                    }

                    if steered {
                        // US-007 AC2: the deltas of this sampling were NEVER
                        // committed, so they are dropped with `acc` and the client
                        // is told to erase what it displayed. The loop head then
                        // re-samples with the steer already in the transcript.
                        // Nothing is persisted here: an abandoned sampling has no
                        // assistant turn to reconcile.
                        if acc.has_visible_output() {
                            yield AgentEvent::StreamReset;
                        }
                        continue;
                    }

                    if let Some(e) = stream_err {
                        record_attempt_usage(
                            &mut usage_budget,
                            &mut budget,
                            last_usage,
                            &messages,
                            static_input_tokens,
                            &acc,
                            &deps,
                        );
                        if e.is_context_error() {
                            if acc.has_visible_output() {
                                yield AgentEvent::StreamReset;
                            }
                            pending = Some(PendingError { kind: ContextErrorKind::PromptTooLong });
                            continue;
                        }
                        let class = deps.provider.classify_error(&e);
                        match class {
                            ErrorClass::Retryable
                            | ErrorClass::RateLimited
                            | ErrorClass::Overloaded(_) => {
                                if maybe_switch_to_overload_fallback(
                                    &mut model,
                                    &config,
                                    &mut overload_fallback_used,
                                    class,
                                ) {
                                    if acc.has_visible_output() {
                                        yield AgentEvent::StreamReset;
                                    }
                                    match rebuild_budget_after_model_switch(
                                        &model,
                                        &config,
                                        &messages,
                                        static_input_tokens,
                                        &deps,
                                    ) {
                                        Ok(next_budget) => budget = next_budget,
                                        Err(e) => {
                                            yield AgentEvent::Error(AgentError::InvalidRequest(e));
                                            return;
                                        }
                                    }
                                    transient_retries = 0;
                                    continue;
                                }
                                if transient_retries >= config.max_retries {
                                    if acc.has_visible_output() {
                                        yield AgentEvent::StreamReset;
                                    }
                                    yield AgentEvent::Error((&e).into());
                                    return;
                                }
                                if acc.has_visible_output() {
                                    yield AgentEvent::StreamReset;
                                }
                                transient_retries += 1;
                                // attempt indexed from 0 -> delays 1x,2x,4x.
                                // US-023: honors Retry-After (max(backoff, retry_after), bounded).
                                // US-001: a backoff can last up to 60 s (bounded
                                // Retry-After); cancellation cuts the wait short, the stop
                                // boundary being the loop head.
                                let _ = guard(
                                    &deps.cancel,
                                    deps.clock.sleep(transient_retry_delay(
                                        &config,
                                        transient_retries - 1,
                                        class,
                                        &e,
                                        deps.clock.now_ms(),
                                    )),
                                )
                                .await;
                                continue;
                            }
                            ErrorClass::Auth(AuthError::Expired) => {
                                if acc.has_visible_output() {
                                    yield AgentEvent::StreamReset;
                                }
                                if transient_retries >= config.max_retries {
                                    yield AgentEvent::Error(AgentError::Auth(AuthError::Expired));
                                    return;
                                }
                                transient_retries += 1;
                                if let Err(refresh_err) = deps.provider.refresh_auth().await {
                                    yield AgentEvent::Error((&refresh_err).into());
                                    return;
                                }
                                continue;
                            }
                            ErrorClass::Auth(a) => {
                                if acc.has_visible_output() {
                                    yield AgentEvent::StreamReset;
                                }
                                yield AgentEvent::Error(AgentError::Auth(a));
                                return;
                            }
                            ErrorClass::InvalidRequest => {
                                if acc.has_visible_output() {
                                    yield AgentEvent::StreamReset;
                                }
                                yield AgentEvent::Error((&e).into());
                                return;
                            }
                        }
                    }

                    transient_retries = 0;
                    model_turns += 1;

                    // Usage fallback: without an `usage` in the stream, estimate
                    // locally to feed the compaction threshold (invariant 7). We also
                    // account the turn in the US-014 budget (real when available,
                    // otherwise estimated: context input + generated output).
                    record_attempt_usage(
                        &mut usage_budget,
                        &mut budget,
                        last_usage,
                        &messages,
                        static_input_tokens,
                        &acc,
                        &deps,
                    );

                    // US-020: structured trace of the round-trip. EMISSION only:
                    // without a subscriber installed by the binary this is an
                    // atomic level check, which keeps the core free of I/O
                    // (ADR-3, invariant 1). No message content here, only counters.
                    tracing::debug!(
                        target: "pyxis::turn",
                        index = model_turns,
                        input_tokens = usage_budget.spent_input(),
                        output_tokens = usage_budget.spent_output(),
                        context_tokens = last_usage.map(|usage| usage.input),
                        "model round-trip ended"
                    );

                    // US-017: a model round-trip just ended. Emitted here, after
                    // accounting, so that the counters carried by the event
                    // include THIS turn. This is the only point of the run
                    // where `model_turns` advances.
                    yield AgentEvent::ModelTurn(crate::event::ModelTurnView {
                        index: model_turns,
                        input_tokens: usage_budget.spent_input(),
                        output_tokens: usage_budget.spent_output(),
                        // US-002: real occupancy of the window, absent when the
                        // provider reported nothing (never reported as zero).
                        context_tokens: last_usage.map(|usage| usage.input),
                        context_window: model_runtime
                            .as_ref()
                            .map(|runtime| runtime.context_window)
                            .or_else(|| deps.provider.context_window_for_model(&model)),
                        auto_compact_token_limit: model_runtime
                            .as_ref()
                            .map(|runtime| runtime.auto_compact_token_limit),
                        estimated_context_tokens: estimated_input,
                    });

                    let transition = post_stream_transition(&acc);
                    let commits_assistant = matches!(
                        transition,
                        Transition::EndTurn | Transition::Continue | Transition::RunTools(_)
                    );
                    if commits_assistant && !acc.is_empty() {
                        messages.push(acc.to_assistant_message());
                    } else if acc.has_visible_output() {
                        yield AgentEvent::StreamReset;
                    }
                    if commits_assistant {
                        compaction.record_success();
                    }

                    transition
                }
            }
            };

            // Exhaustive match over every transition, checked at compile time.
            match transition {
                Transition::Continue => {}
                Transition::EndTurn => {
                    // US-024: persistence of the LAST assistant turn. The final
                    // assistant message (acc.to_assistant_message) was just pushed,
                    // but the loop-head sync would only run on the NEXT turn, which
                    // will not happen. Final sync (delta-only, idempotent) before
                    // handing back control, otherwise `/resume` loses the last reply.
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }
                    yield AgentEvent::EndTurn;
                    return;
                }
                Transition::RunTools(calls) => {
                    // transcript-before-response for the ASSISTANT TURN: the assistant
                    // message (with its tool_use, already pushed) is persisted BEFORE
                    // running the tools. Otherwise a crash during the dispatch would
                    // leave orphan tool_results (without an assistant turn) at
                    // resume: a structurally invalid transcript (#1).
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }

                    // US-014: deterministic loop guardrail (FR-05). It OVERRIDES the
                    // model's logic. At the threshold -> signal without executing;
                    // past it -> deterministic stop (iter_cap stays the last resort).
                    match loop_guard.observe(batch_signature(&calls)) {
                        LoopDecision::Abort => {
                            yield AgentEvent::Exhausted(ExhaustReason::ToolLoop {
                                count: loop_guard.count(),
                            });
                            return;
                        }
                        LoopDecision::Signal => {
                            // Hard stop on the repeated batch: we DO NOT EXECUTE, we send
                            // an explicit signal back to the agent (edge case #2). One
                            // tool_result per tool_use -> valid transcript.
                            for c in &calls {
                                let msg = format!(
                                    "Loop detected on {} (x{}). Stopping. Reframe the approach \
                                     or ask for intervention.",
                                    c.name,
                                    loop_guard.count(),
                                );
                                let result = ModelToolResult::new(
                                    c.id.clone(),
                                    msg,
                                    true,
                                    false,
                                    Some(crate::message::ToolErrorKind::Semantic),
                                );
                                yield AgentEvent::ToolResult(ToolResultView::from_model(&result));
                                messages.push(Message::tool_result_from_model(&result));
                            }
                            // loop back: the model gets the signal and can correct itself.
                        }
                        LoopDecision::Proceed => {
                            for c in &calls {
                                yield AgentEvent::ToolCall(ToolCallView {
                                    id: c.id.clone(),
                                    name: c.name.clone(),
                                    input: c.input.clone(),
                                });
                            }
                            let (tool_event_tx, mut tool_event_rx) =
                                tokio::sync::mpsc::unbounded_channel();
                            let expected_ids: Vec<ToolCallId> =
                                calls.iter().map(|c| c.id.clone()).collect();
                            let dispatch =
                                tool_plan.dispatch(calls, ToolEventSink::new(tool_event_tx));
                            tokio::pin!(dispatch);
                            let mut tool_events_open = true;
                            // US-001: `biased` with the dispatch FIRST. A batch that just
                            // finished yields its real results instead of being discarded
                            // by a cancellation that landed in the same window (edge
                            // case #2). Otherwise the loop takes control back without
                            // waiting for the tools, which are abandoned along with the
                            // dispatch future.
                            let outcomes = loop {
                                tokio::select! {
                                    biased;
                                    outcomes = &mut dispatch => break Some(outcomes),
                                    event = tool_event_rx.recv(), if tool_events_open => {
                                        match event {
                                            Some(ToolDispatchEvent::PermissionAsk(req)) => {
                                                yield AgentEvent::PermissionAsk(req);
                                            }
                                            Some(ToolDispatchEvent::OutputDelta { id, chunk }) => {
                                                yield AgentEvent::ToolOutputDelta(ToolOutputDeltaView { id, chunk });
                                            }
                                            Some(ToolDispatchEvent::Plan(view)) => {
                                                yield AgentEvent::Plan(view);
                                            }
                                            None => tool_events_open = false,
                                        }
                                    }
                                    () = deps.cancel.cancelled() => break None,
                                }
                            };
                            let Some(outcomes) = outcomes else {
                                // Abandoned in-flight calls: the loop head
                                // reconciles each of them before persisting (US-002).
                                continue;
                            };
                            let (feedback_tokens, feedback_bytes) =
                                feedback_limits(&model_runtime);
                            let outcomes: Vec<ModelToolResult> = outcomes
                                .into_iter()
                                .map(|outcome| {
                                    outcome.bound_feedback(
                                        deps.tokenizer.as_ref(),
                                        feedback_tokens,
                                        feedback_bytes,
                                    )
                                })
                                .collect();
                            while let Ok(event) = tool_event_rx.try_recv() {
                                match event {
                                    ToolDispatchEvent::PermissionAsk(req) => {
                                        yield AgentEvent::PermissionAsk(req);
                                    }
                                    ToolDispatchEvent::OutputDelta { id, chunk } => {
                                        yield AgentEvent::ToolOutputDelta(ToolOutputDeltaView { id, chunk });
                                    }
                                    ToolDispatchEvent::Plan(view) => {
                                        yield AgentEvent::Plan(view);
                                    }
                                }
                            }
                            if let Err(e) = validate_tool_outcomes(&expected_ids, &outcomes) {
                                yield AgentEvent::Error(e);
                                return;
                            }
                            for o in &outcomes {
                                yield AgentEvent::ToolResult(ToolResultView::from_model(o));
                                messages.push(Message::tool_result_from_model(o));
                                // US-011: a `tool_result` block carries text only. The
                                // images the tool read therefore enter as a user message
                                // right after it, which is also what makes them
                                // strippable by the full compaction (already
                                // implemented, section 5).
                                if !o.images.is_empty() {
                                    let mut blocks: Vec<crate::message::ContentBlock> =
                                        Vec::with_capacity(o.images.len());
                                    for image in &o.images {
                                        blocks.push(crate::message::ContentBlock::Image {
                                            media_type: image.media_type.clone(),
                                            data: image.data.clone(),
                                        });
                                    }
                                    messages.push(Message {
                                        role: crate::message::Role::User,
                                        content: blocks,
                                    });
                                }
                            }
                            // US-030 MidTurn: the tool_results we just added are NOT
                            // in the budget yet (it is based on the previous turn's
                            // usage). We PROJECT their weight (without overwriting the
                            // real budget); if a long result crosses the threshold, we
                            // force compaction on the next turn, before the model.
                            let projected = estimate_current_input(&messages, static_input_tokens, &deps);
                            if budget.would_autocompact(projected) {
                                force_compact = true;
                            }
                            // loop back: the model sees the results.
                        }
                    }
                }
                Transition::Compact(kind) => {
                    // US-001: compaction is a full model call; cancellation does not
                    // wait for it. The transcript is not modified until
                    // `full_compact` has handed back control.
                    let compacted = match guard(
                        &deps.cancel,
                        full_compact(
                            &mut messages,
                            &model,
                            deps.provider.as_ref(),
                            config.max_output_tokens,
                        ),
                    )
                    .await
                    {
                        Cancellable::Cancelled => continue,
                        Cancellable::Completed(compacted) => compacted,
                    };
                    match compacted {
                        Ok(usage) => {
                            usage_budget.record_usage(usage);
                            compaction.record_success();
                            // ATOMIC checkpoint: boundary + summarized transcript in
                            // one operation; I/O error propagated (no let _ that would
                            // desynchronize the session cursor, #8).
                            if let Err(e) = deps.session.checkpoint(kind, &messages).await {
                                yield AgentEvent::Error(AgentError::Session(e.to_string()));
                                return;
                            }
                            // US-030: anchors the baseline on the NEXT real usage
                            // (guards against an immediate double compaction).
                            let compacted_input = estimate_current_input(&messages, static_input_tokens, &deps);
                            budget.mark_compacted(compacted_input);
                            yield AgentEvent::Compacted(kind);
                        }
                        Err(_) => {
                            let n = compaction.record_failure();
                            if compaction.tripped(config.compaction_breaker_limit) {
                                yield AgentEvent::Error(AgentError::CompactionCircuitBreaker(n));
                                return;
                            }
                            // anti error-loop: structural microcompact to lower the
                            // pressure before looping back.
                            let pruned = microcompact(&mut messages, config.micro_keep_recent);
                            if pruned > 0 {
                                compaction.record_success();
                                yield AgentEvent::Compacted(CompactKind::Micro);
                            }
                            budget.observe_estimated(estimate_current_input(&messages, static_input_tokens, &deps));
                        }
                    }
                }
                Transition::Recover(_) => {
                    // withholding: REACTIVE compaction; confirmed failure -> propagation.
                    let compacted = match guard(
                        &deps.cancel,
                        full_compact(
                            &mut messages,
                            &model,
                            deps.provider.as_ref(),
                            config.max_output_tokens,
                        ),
                    )
                    .await
                    {
                        Cancellable::Cancelled => continue,
                        Cancellable::Completed(compacted) => compacted,
                    };
                    match compacted {
                        Ok(usage) => {
                            usage_budget.record_usage(usage);
                            compaction.record_success();
                            if let Err(e) =
                                deps.session.checkpoint(CompactKind::Reactive, &messages).await
                            {
                                yield AgentEvent::Error(AgentError::Session(e.to_string()));
                                return;
                            }
                            let compacted_input = estimate_current_input(&messages, static_input_tokens, &deps);
                            budget.mark_compacted(compacted_input);
                            yield AgentEvent::Compacted(CompactKind::Reactive);
                        }
                        Err(e) => {
                            yield AgentEvent::Error(AgentError::ContextUnrecoverable(e.to_string()));
                            return;
                        }
                    }
                }
                Transition::Interrupted => {
                    // US-002: reconciliation BEFORE persistence, every call left
                    // unanswered gets an explicit result, otherwise the next turn
                    // re-emits an orphan `function_call` that the backend rejects.
                    for view in reconcile_interrupted_calls(&mut messages) {
                        yield AgentEvent::ToolResult(view);
                    }
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }
                    // US-001: the event is emitted by the CORE, the client no longer
                    // has to build it after an abort decided from the outside.
                    yield AgentEvent::Interrupted;
                    return;
                }
                Transition::Exhausted(reason) => {
                    yield AgentEvent::Exhausted(reason);
                    return;
                }
                Transition::Fail(e) => {
                    yield AgentEvent::Error(e);
                    return;
                }
            }
        }
    }
}

// ───────────────────────── Headless mode (`-p`) ─────────────────────────

#[derive(Debug)]
pub enum HeadlessEnd {
    EndTurn,
    Exhausted(ExhaustReason),
    Error(AgentError),
}

#[derive(Debug)]
pub struct HeadlessResult {
    pub text: String,
    pub events: usize,
    pub ended: HeadlessEnd,
}

/// Consumes the loop in headless mode: aggregates text, NO Ratatui (AC3).
/// This is what `pyxis -p` will wire up (agent-cli); here, testable without a terminal.
pub async fn run_headless(ctx: AgentContext, deps: Deps) -> HeadlessResult {
    run_headless_observed(ctx, deps, |_| {}).await
}

/// Same loop, with an observer called on EVERY event in emission order
/// (US-017: JSONL output). Text aggregation stays here, in a single place:
/// a client that wants both does not reimplement the consolidation rule,
/// whose silence on `Text` before a `ToolCall` is subtle.
/// The observer performs no async I/O: it writes a line and hands back
/// control, which keeps the core free of any output dependency.
pub async fn run_headless_observed(
    ctx: AgentContext,
    deps: Deps,
    mut observe: impl FnMut(&AgentEvent),
) -> HeadlessResult {
    let stream = run_agent(ctx, deps);
    futures_util::pin_mut!(stream);

    let mut text = String::new();
    let mut pending_text = String::new();
    let mut events = 0usize;
    let mut ended = HeadlessEnd::EndTurn;

    while let Some(ev) = stream.next().await {
        events += 1;
        observe(&ev);
        match ev {
            AgentEvent::StreamReset => pending_text.clear(),
            AgentEvent::Text(t) => pending_text.push_str(&t),
            AgentEvent::ToolCall(_) => {
                text.push_str(&pending_text);
                pending_text.clear();
            }
            AgentEvent::Exhausted(r) => ended = HeadlessEnd::Exhausted(r),
            AgentEvent::Error(e) => ended = HeadlessEnd::Error(e),
            AgentEvent::EndTurn => {
                text.push_str(&pending_text);
                pending_text.clear();
                ended = HeadlessEnd::EndTurn;
            }
            _ => {}
        }
    }
    HeadlessResult {
        text,
        events,
        ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(retry_after_ms: Option<u64>) -> ProviderError {
        ProviderError::Http {
            status: 429,
            message: String::new(),
            retry_after_ms,
        }
    }

    // US-023: without a server header -> backoff alone.
    #[test]
    fn retry_delay_without_header_uses_backoff() {
        let base = Duration::from_millis(50);
        assert_eq!(retry_delay(base, &http(None)), base);
        assert_eq!(
            retry_delay(base, &ProviderError::Transport("x".into())),
            base
        );
    }

    // US-023: Retry-After longer than the backoff -> it wins.
    #[test]
    fn retry_delay_honors_longer_retry_after() {
        let base = Duration::from_millis(50);
        assert_eq!(
            retry_delay(base, &http(Some(2_000))),
            Duration::from_millis(2_000)
        );
    }

    // US-023: backoff longer than Retry-After -> the backoff wins (max).
    #[test]
    fn retry_delay_keeps_longer_backoff() {
        let base = Duration::from_millis(5_000);
        assert_eq!(retry_delay(base, &http(Some(1_000))), base);
    }

    // US-023: an absurd Retry-After is bounded (never an indefinite freeze).
    #[test]
    fn retry_delay_caps_absurd_retry_after() {
        let base = Duration::from_millis(50);
        assert_eq!(
            retry_delay(base, &http(Some(3_600_000))),
            Duration::from_millis(MAX_RETRY_AFTER_MS)
        );
    }

    #[test]
    fn overloaded_retry_uses_longer_base_delay() {
        let cfg = RunConfig {
            backoff_base_ms: 10,
            ..RunConfig::default()
        };
        let err = ProviderError::Http {
            status: 529,
            message: String::new(),
            retry_after_ms: None,
        };
        let overloaded = transient_retry_delay(&cfg, 0, ErrorClass::Overloaded(529), &err, 0);
        let retryable = transient_retry_delay(&cfg, 0, ErrorClass::Retryable, &err, 0);
        assert!(overloaded > Duration::from_millis(30));
        assert!(overloaded <= Duration::from_millis(36));
        assert!(retryable > Duration::from_millis(10));
        assert!(retryable <= Duration::from_millis(12));
    }

    #[test]
    fn retry_after_cap_is_not_extended_by_jitter() {
        let cfg = RunConfig {
            backoff_base_ms: 10,
            ..RunConfig::default()
        };
        let err = http(Some(3_600_000));
        assert_eq!(
            transient_retry_delay(&cfg, 0, ErrorClass::RateLimited, &err, 0),
            Duration::from_millis(MAX_RETRY_AFTER_MS)
        );
    }

    #[test]
    fn overload_fallback_switches_once() {
        let cfg = RunConfig {
            overload_fallback_model: Some("fallback".into()),
            ..RunConfig::default()
        };
        let mut model = "primary".to_string();
        let mut used = false;
        assert!(maybe_switch_to_overload_fallback(
            &mut model,
            &cfg,
            &mut used,
            ErrorClass::Overloaded(529)
        ));
        assert_eq!(model, "fallback");
        assert!(!maybe_switch_to_overload_fallback(
            &mut model,
            &cfg,
            &mut used,
            ErrorClass::Overloaded(529)
        ));
    }

    // backoff: exponential capped at 32x (2^5), no overflow.
    #[test]
    fn backoff_is_exponential_capped() {
        let cfg = RunConfig {
            backoff_base_ms: 10,
            ..RunConfig::default()
        };
        assert_eq!(backoff(&cfg, 0), Duration::from_millis(10));
        assert_eq!(backoff(&cfg, 1), Duration::from_millis(20));
        assert_eq!(backoff(&cfg, 2), Duration::from_millis(40));
        // past 2^5 the factor is pinned at 32.
        assert_eq!(backoff(&cfg, 5), Duration::from_millis(320));
        assert_eq!(backoff(&cfg, 50), Duration::from_millis(320));
    }
}
