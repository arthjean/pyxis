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
use crate::event::{
    AgentEvent, CredentialRefreshOutcome, CredentialRefreshView, InterruptReason,
    RetryScheduledView, ToolCallView, ToolResultView,
};
use crate::guardrail::{
    CostBudget, LOOP_GUARD_THRESHOLDS, LoopDecision, LoopGuard, UsageBudget,
    guarded_batch_signature, loop_guard_message,
};
use crate::input::InputQueue;
use crate::message::{
    ContentBlock, INTERRUPTED_TOOL_RESULT, Message, ToolCallId, unanswered_tool_calls,
};
use crate::model::{ResolvedModelRuntime, TruncationMode};
use crate::prompt::{ContextTransitionCause, PromptSnapshot, replay_enabled, transition_between};
use crate::provider::{
    AuthError, ErrorClass, ProviderError, StreamEvent, TURN_ID_METADATA_KEY, TokenUsage, ToolSpec,
};
use crate::step::{ContextFragment, StepContextSource};
use crate::tools::{
    MAX_MODEL_TOOL_RESULT_BYTES, ModelToolResult, StepToolPlan, ToolDispatchSnapshot, ToolEventSink,
};
use crate::transition::{
    Accumulator, ContextErrorKind, ExhaustReason, PendingError, Transition, post_stream_transition,
    pre_stream_transition,
};

/// Loop settings (guardrails, thresholds).
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub max_output_tokens: u32,
    pub max_retries: u32,
    pub micro_keep_recent: usize,
    pub compaction_breaker_limit: u32,
    pub backoff_base_ms: u64,
    /// US-014: cumulated token budget (kill-switch). `None` = disabled.
    pub token_budget: Option<u64>,
    /// US-014: cumulated cost budget (kill-switch). `None` = disabled.
    pub cost_budget: Option<CostBudget>,
    /// Optional fallback model after a provider overload.
    pub overload_fallback_model: Option<String>,
    /// Fully resolved fallback contract. Required when the primary turn already
    /// carries a resolved runtime; a bare slug is accepted only on legacy paths.
    pub overload_fallback_runtime: Option<ResolvedModelRuntime>,
    /// Calibration probe (US-002): when enabled, every model round-trip carries
    /// the local estimate of its input next to the backend measure, so a client
    /// can compare them. Off by default: the estimate costs a tokenizer pass
    /// over the whole transcript.
    pub usage_probe: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: 4096,
            max_retries: 3,
            micro_keep_recent: 2,
            compaction_breaker_limit: 3,
            backoff_base_ms: 50,
            token_budget: None,
            cost_budget: None,
            overload_fallback_model: None,
            overload_fallback_runtime: None,
            usage_probe: false,
        }
    }
}

/// Context of an agent run (model, system, transcript, tools).
pub struct AgentContext {
    /// Durable turn identity when the loop is driven by `agent-runtime`.
    /// Direct embedders may leave it absent.
    pub turn_id: Option<String>,
    pub model: String,
    /// Immutable model contract captured before the turn starts.
    pub model_runtime: Option<ResolvedModelRuntime>,
    pub reasoning_effort: Option<String>,
    pub system: Option<String>,
    /// Effective instructions precomposed for the resolved overload fallback.
    /// Kept beside the runtime so a switch never reuses the primary contract.
    pub overload_fallback_system: Option<String>,
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
            turn_id: None,
            model: model.into(),
            model_runtime: None,
            reasoning_effort: None,
            system: None,
            overload_fallback_system: None,
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

#[derive(Debug, Clone, Copy)]
struct ResolvedAttemptPolicy {
    max_attempts: u32,
    backoff_base_ms: u64,
}

impl ResolvedAttemptPolicy {
    fn resolve(config: &RunConfig, runtime: Option<&ResolvedModelRuntime>) -> Self {
        match runtime {
            Some(runtime) => Self {
                max_attempts: runtime.retry.max_attempts.max(1),
                backoff_base_ms: runtime.retry.backoff_base_ms,
            },
            None => Self {
                max_attempts: config.max_retries.saturating_add(1).max(1),
                backoff_base_ms: config.backoff_base_ms,
            },
        }
    }

    fn permits_after(self, current_ordinal: u32) -> bool {
        current_ordinal < self.max_attempts
    }
}

#[derive(Debug, Clone)]
struct AttemptContext {
    turn_id: Option<String>,
    step: u32,
    prompt_fingerprint: String,
    model_runtime_fingerprint: String,
    tool_plan_fingerprint: String,
}

impl AttemptContext {
    fn retry_scheduled(
        &self,
        ordinal: u32,
        policy: ResolvedAttemptPolicy,
        cause: ErrorClass,
        delay: Duration,
        fallback_model: Option<String>,
    ) -> AgentEvent {
        AgentEvent::RetryScheduled(RetryScheduledView {
            turn_id: self.turn_id.clone(),
            step: self.step,
            ordinal,
            max_attempts: policy.max_attempts,
            cause,
            delay_ms: delay.as_millis().min(u64::MAX as u128) as u64,
            fallback_model,
            prompt_fingerprint: self.prompt_fingerprint.clone(),
            model_runtime_fingerprint: self.model_runtime_fingerprint.clone(),
            tool_plan_fingerprint: self.tool_plan_fingerprint.clone(),
        })
    }

    fn credential_refresh(
        &self,
        attempt_ordinal: u32,
        outcome: CredentialRefreshOutcome,
    ) -> AgentEvent {
        AgentEvent::CredentialRefresh(CredentialRefreshView {
            turn_id: self.turn_id.clone(),
            step: self.step,
            attempt_ordinal,
            outcome,
        })
    }
}

fn backoff(policy: ResolvedAttemptPolicy, attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(5);
    Duration::from_millis(policy.backoff_base_ms.saturating_mul(factor))
}

/// Cap on the honored `Retry-After` delay (US-023). A server cannot freeze the
/// loop forever: an absurd delay is bounded, we retry then give up according
/// to `max_attempts`. Same cap as Pi (60 s).
const MAX_RETRY_AFTER_MS: u64 = 60_000;

/// Effective retry delay (US-023): `max(exponential backoff, Retry-After)`, the
/// server delay (exact ms) winning when it is longer, bounded by
/// `MAX_RETRY_AFTER_MS`. Errors without a server header fall back on the backoff.
fn retry_delay(base: Duration, err: &ProviderError) -> Duration {
    match err {
        ProviderError::Http {
            retry_after_ms: Some(ms),
            ..
        }
        | ProviderError::Api {
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
        ErrorClass::ContextLimit => 6,
        ErrorClass::ReasoningReplayRejected => 5,
        ErrorClass::Auth(_) => 3,
        ErrorClass::InvalidRequest => 4,
    };
    let status_code = match err {
        ProviderError::Http { status, .. } => *status as u64,
        ProviderError::Api { status, .. } => status.map(u64::from).unwrap_or(16),
        ProviderError::Transport(_) => 10,
        ProviderError::Decode(_) => 11,
        ProviderError::Stream(_) => 12,
        ProviderError::ContextLengthExceeded => 13,
        ProviderError::Credential(_) => 14,
        ProviderError::UnsupportedTool { .. } => 15,
        ProviderError::UnsupportedCapability { .. } => 17,
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

fn recovery_failure(error: &ProviderError) -> (CredentialRefreshOutcome, AuthError) {
    match error {
        ProviderError::Credential(AuthError::RecoveryPermanent) => (
            CredentialRefreshOutcome::Permanent,
            AuthError::RecoveryPermanent,
        ),
        ProviderError::Credential(AuthError::RecoveryTransient) => (
            CredentialRefreshOutcome::Transient,
            AuthError::RecoveryTransient,
        ),
        ProviderError::Credential(AuthError::RecoveryUnavailable) => (
            CredentialRefreshOutcome::Unavailable,
            AuthError::RecoveryUnavailable,
        ),
        ProviderError::Credential(error) => (CredentialRefreshOutcome::Rejected, *error),
        _ => (
            CredentialRefreshOutcome::Rejected,
            AuthError::ReconnectRequired,
        ),
    }
}

fn transient_retry_delay(
    policy: ResolvedAttemptPolicy,
    attempt: u32,
    class: ErrorClass,
    err: &ProviderError,
    now_ms: u64,
) -> Duration {
    let mut base = backoff(policy, attempt);
    if matches!(class, ErrorClass::Overloaded(_)) {
        base = base.saturating_mul(3);
    }
    let delay = retry_delay(base, err);
    if matches!(
        err,
        ProviderError::Http {
            retry_after_ms: Some(ms),
            ..
        } | ProviderError::Api {
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

fn classified_provider_error(class: ErrorClass, error: &ProviderError) -> AgentError {
    AgentError::Provider(ProviderFailure::classified(error, class))
}

fn validate_tool_outcomes(
    expected_ids: &[ToolCallId],
    outcomes: &[ModelToolResult],
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

fn invalidate_reasoning(messages: &mut [Message]) -> bool {
    let mut removed = false;
    for message in messages {
        message.content.retain(|block| {
            let keep = !matches!(block, ContentBlock::EncryptedReasoning { .. });
            removed |= !keep;
            keep
        });
    }
    removed
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

/// Model contract and context state of the running turn.
///
/// Everything a provider failure can rewrite lives here. The overload fallback
/// replaces a model, its instructions, its output limit, its tool geometry and
/// its budget at once; keeping those fields together is what makes that switch
/// one operation instead of nine assignments a caller could half-apply.
struct TurnRuntime {
    model: String,
    model_runtime: Option<ResolvedModelRuntime>,
    reasoning_effort: Option<String>,
    system: Option<String>,
    /// Effective instructions precomposed for the resolved overload fallback.
    overload_fallback_system: Option<String>,
    parallel_tools: bool,
    tool_plan: StepToolPlan,
    budget: ContextBudget,
    /// System prompt, step context, tool schemas and ephemeral fragments: the
    /// overhead the backend counts in `usage.input` beside the transcript.
    static_input_tokens: u32,
    reasoning_replay: bool,
    reasoning_replay_downgraded: bool,
    overload_fallback_used: bool,
    credential_refresh_attempted: bool,
    attempt_policy: ResolvedAttemptPolicy,
    /// 1-based provider opening within the current sampling.
    attempt_ordinal: u32,
    /// Slug whose compaction profile the transcript still carries, when a
    /// `comp_hash` change forces a compaction before the next sampling.
    required_profile_compaction: Option<String>,
    transition_causes: Vec<ContextTransitionCause>,
}

impl TurnRuntime {
    fn recompute_static_input(
        &mut self,
        context: &[ContextFragment],
        ephemeral_messages: &[Message],
        deps: &Deps,
    ) {
        self.static_input_tokens = estimate_static_input(
            &self.system,
            context.iter().map(|fragment| &fragment.message),
            self.tool_plan.specs(),
            deps.tokenizer.as_ref(),
        )
        .saturating_add(estimate_input(ephemeral_messages, deps.tokenizer.as_ref()));
    }

    /// Feeds the compaction threshold with the local projection of the current
    /// transcript. Used wherever no backend `usage` is available to supersede it.
    fn observe_estimated(&mut self, messages: &[Message], deps: &Deps) {
        let estimated = estimate_current_input(messages, self.static_input_tokens, deps);
        self.budget.observe_estimated(estimated);
    }

    fn requires_profile_compaction(&self) -> bool {
        self.required_profile_compaction.is_some()
    }

    /// Model the compaction summary must be produced by: the profile the
    /// transcript was written under, which is not always the active one.
    fn compaction_model(&self) -> &str {
        self.required_profile_compaction
            .as_deref()
            .unwrap_or(self.model.as_str())
    }

    /// Installs the overload fallback contract, at most once per run.
    ///
    /// `None` when nothing can take over, `Some(Err)` when the configured
    /// fallback is itself invalid (a configuration error, not a retry). The
    /// budget is built BEFORE anything is assigned, so a rejected geometry
    /// leaves the turn on its original contract.
    fn switch_to_overload_fallback(
        &mut self,
        config: &mut RunConfig,
        class: ErrorClass,
        context: &[ContextFragment],
        ephemeral_messages: &[Message],
        messages: &[Message],
        deps: &Deps,
    ) -> Option<Result<(), String>> {
        if !matches!(class, ErrorClass::Overloaded(_)) || self.overload_fallback_used {
            return None;
        }

        // A resolved contract carries its own window, instructions and tool
        // geometry, so switching to it is a complete substitution.
        if let Some(previous) = self.model_runtime.clone()
            && let Some(fallback) = config
                .overload_fallback_runtime
                .as_ref()
                .filter(|fallback| fallback.fingerprint != previous.fingerprint)
                .cloned()
        {
            if let Err(error) = fallback.validate() {
                return Some(Err(error.to_string()));
            }
            let budget = match ContextBudget::try_for_model_with_auto_limit(
                fallback.context_window,
                fallback.max_output_tokens,
                Some(fallback.auto_compact_token_limit),
            ) {
                Ok(budget) => budget,
                Err(error) => return Some(Err(error)),
            };

            self.model = fallback.slug.clone();
            self.reasoning_effort = fallback.reasoning_effort.clone();
            self.system = self
                .overload_fallback_system
                .clone()
                .or_else(|| Some(fallback.instructions.clone()));
            config.max_output_tokens = fallback.max_output_tokens;
            self.parallel_tools = fallback.supports_parallel_tool_calls;
            self.tool_plan = self.tool_plan.with_parallel_allowed(self.parallel_tools);
            self.budget = budget;
            self.transition_causes
                .push(ContextTransitionCause::OverloadFallback);
            // A different compaction profile means the transcript must be
            // summarized by the OLD model before the new one ever reads it.
            if previous.comp_hash != fallback.comp_hash {
                self.required_profile_compaction = Some(previous.slug.clone());
                self.transition_causes.extend([
                    ContextTransitionCause::CompHashChanged,
                    ContextTransitionCause::Compaction,
                ]);
            }
            self.model_runtime = Some(fallback);
            self.reasoning_replay =
                !self.reasoning_replay_downgraded && replay_enabled(self.model_runtime.as_ref());
            self.overload_fallback_used = true;
            self.recompute_static_input(context, ephemeral_messages, deps);
            self.observe_estimated(messages, deps);
            return Some(Ok(()));
        }

        // Legacy path: a bare slug, with nothing but the provider's declared
        // window to rebuild a budget from.
        let fallback = config
            .overload_fallback_model
            .as_deref()
            .map(str::trim)
            .filter(|fallback| !fallback.is_empty() && *fallback != self.model)?
            .to_string();
        self.budget = match ContextBudget::try_for_model(
            deps.provider.max_context_for_model(&fallback),
            config.max_output_tokens,
        ) {
            Ok(budget) => budget,
            Err(error) => return Some(Err(error)),
        };
        self.model = fallback;
        self.overload_fallback_used = true;
        self.observe_estimated(messages, deps);
        Some(Ok(()))
    }

    /// Decides what a failed sampling leads to. Taken in ONE place: a failure
    /// when opening the stream and a failure while draining it differ in what
    /// they observed, never in what they conclude.
    fn plan_failure(
        &mut self,
        error: &ProviderError,
        config: &mut RunConfig,
        context: &[ContextFragment],
        ephemeral_messages: &[Message],
        messages: &[Message],
        deps: &Deps,
    ) -> (ErrorClass, FailureAction) {
        // Withholding (invariant 8): a context error is answered by compaction,
        // never by a bare retry, because reopening the same oversized prompt
        // fails the same way.
        if error.is_context_error() {
            let action = if self.attempt_policy.permits_after(self.attempt_ordinal) {
                FailureAction::Withhold(ContextErrorKind::PromptTooLong)
            } else {
                FailureAction::Fail(classified_provider_error(ErrorClass::ContextLimit, error))
            };
            return (ErrorClass::ContextLimit, action);
        }

        let class = deps.provider.classify_error(error);
        let action = match class {
            ErrorClass::Retryable | ErrorClass::RateLimited | ErrorClass::Overloaded(_) => {
                if !self.attempt_policy.permits_after(self.attempt_ordinal) {
                    return (
                        class,
                        FailureAction::Fail(classified_provider_error(class, error)),
                    );
                }
                match self.switch_to_overload_fallback(
                    config,
                    class,
                    context,
                    ephemeral_messages,
                    messages,
                    deps,
                ) {
                    Some(Err(invalid)) => FailureAction::Fail(AgentError::InvalidRequest(invalid)),
                    // Another model answers right away: the wait the overload
                    // justified is exactly what the switch removes.
                    Some(Ok(())) => FailureAction::Retry {
                        delay: Duration::ZERO,
                        fallback_model: Some(self.model.clone()),
                    },
                    None => FailureAction::Retry {
                        delay: transient_retry_delay(
                            self.attempt_policy,
                            self.attempt_ordinal - 1,
                            class,
                            error,
                            deps.clock.now_ms(),
                        ),
                        fallback_model: None,
                    },
                }
            }
            ErrorClass::ReasoningReplayRejected => {
                if self.reasoning_replay
                    && !self.reasoning_replay_downgraded
                    && self.attempt_policy.permits_after(self.attempt_ordinal)
                {
                    self.reasoning_replay = false;
                    self.reasoning_replay_downgraded = true;
                    FailureAction::DowngradeReplay
                } else {
                    FailureAction::Fail(classified_provider_error(class, error))
                }
            }
            ErrorClass::Auth(AuthError::Expired) => {
                if self.credential_refresh_attempted
                    || !self.attempt_policy.permits_after(self.attempt_ordinal)
                {
                    FailureAction::Fail(AgentError::Auth(AuthError::ReconnectRequired))
                } else {
                    self.credential_refresh_attempted = true;
                    FailureAction::RefreshCredentials
                }
            }
            ErrorClass::Auth(auth) => FailureAction::Fail(AgentError::Auth(auth)),
            ErrorClass::ContextLimit | ErrorClass::InvalidRequest => {
                FailureAction::Fail(classified_provider_error(class, error))
            }
        };
        (class, action)
    }
}

/// What the loop does about a failed sampling.
enum FailureAction {
    /// Reopen after `delay`, on `fallback_model` when the overload fallback
    /// took over.
    Retry {
        delay: Duration,
        fallback_model: Option<String>,
    },
    /// Refresh the expired credential, then reopen.
    RefreshCredentials,
    /// Reopen without encrypted reasoning replay.
    DowngradeReplay,
    /// Hold the context error back: the loop head compacts, then retries.
    Withhold(ContextErrorKind),
    /// Nothing left to try.
    Fail(AgentError),
}

/// Starts the agent. Returns a `Stream<AgentEvent>` to consume (TUI, `-p`).
pub fn run_agent(ctx: AgentContext, deps: Deps) -> impl Stream<Item = AgentEvent> + Send {
    async_stream::stream! {
        let AgentContext {
            turn_id,
            mut model,
            model_runtime,
            mut reasoning_effort,
            mut system,
            overload_fallback_system,
            mut messages,
            tools,
            mut config,
            context_messages,
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
            if system.is_none() {
                system = Some(runtime.instructions.clone());
            }
            config.max_output_tokens = runtime.max_output_tokens;
            config.overload_fallback_model = None;
        }

        let parallel_tools = model_runtime
            .as_ref()
            .is_none_or(|runtime| runtime.supports_parallel_tool_calls);
        let tool_plan = StepToolPlan::capture(
            ToolDispatchSnapshot::new(0, tools, Arc::clone(&deps.tools)),
            parallel_tools,
        );
        let mut active_tool_plan = tool_plan.clone();

        // ContextBudget computed for the active model (recomputed on overload fallback).
        let max_context = model_runtime
            .as_ref()
            .map(|runtime| runtime.context_window)
            .unwrap_or_else(|| deps.provider.max_context_for_model(&model));
        let auto_compact_token_limit = model_runtime
            .as_ref()
            .map(|runtime| runtime.auto_compact_token_limit);
        let budget = match ContextBudget::try_for_model_with_auto_limit(
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
        // US-006: generation of the step frame currently installed. `None` until
        // the first frame, so a source whose first generation is 0 is still read.
        let mut step_generation: Option<u64> = None;
        let mut context_baseline = deps.session.context_baseline();
        // A transcript written under another compaction profile has to be
        // summarized by the model that wrote it, before the active one reads it.
        let required_profile_compaction = context_baseline.as_ref().and_then(|previous| {
            model_runtime.as_ref().and_then(|runtime| {
                (previous.comp_hash != runtime.comp_hash).then(|| previous.model_slug.clone())
            })
        });
        let transition_causes = if required_profile_compaction.is_some() {
            vec![
                ContextTransitionCause::CompHashChanged,
                ContextTransitionCause::Compaction,
            ]
        } else {
            Vec::new()
        };
        let mut turn = TurnRuntime {
            attempt_policy: ResolvedAttemptPolicy::resolve(&config, model_runtime.as_ref()),
            attempt_ordinal: 1,
            reasoning_replay: replay_enabled(model_runtime.as_ref()),
            reasoning_replay_downgraded: false,
            overload_fallback_used: false,
            credential_refresh_attempted: false,
            model,
            model_runtime,
            reasoning_effort,
            system,
            overload_fallback_system,
            parallel_tools,
            tool_plan,
            budget,
            static_input_tokens: 0,
            required_profile_compaction,
            transition_causes,
        };
        // Backend usage counts everything that is sent: system, ephemeral
        // context, tool schemas and transcript. Local projections must carry
        // the same static overhead, otherwise compaction comes too late.
        // The context handed in by the caller is unclassified, which is what
        // project context is: a skill only ever arrives through a step frame.
        let mut context: Vec<ContextFragment> = context_messages
            .into_iter()
            .map(ContextFragment::project)
            .collect();
        turn.recompute_static_input(&context, &ephemeral_messages, &deps);
        let mut compaction = CompactionState::default();
        let mut pending: Option<PendingError> = None;
        let mut model_turns: u32 = 0;
        // Element-wise cumulation of the usages the backend actually REPORTED.
        // Deliberately separate from `usage_budget`, which also folds in local
        // estimates: mixing the two would let a guess masquerade as a measure.
        let mut run_usage = TokenUsage::default();
        let run_started_ms = deps.clock.now_ms();
        // Armed by a tool the user refused with "stop the turn"; consumed at the
        // next loop boundary so the stop never happens mid-dispatch.
        let mut pending_abort: Option<InterruptReason> = None;
        // US-014: deterministic guardrails (override the model's own logic).
        let mut loop_guard = LoopGuard::new(LOOP_GUARD_THRESHOLDS);
        let mut usage_budget = UsageBudget::new(config.token_budget, config.cost_budget);
        // US-030 (MidTurn): armed when a long tool_result crosses the threshold ->
        // forces compaction on the next turn, BEFORE calling the model again.
        let mut force_compact = false;

        // No iteration ceiling: it was derived from the turn cap and would have
        // been the same limit under another name. What ends this loop is a
        // terminal decision (end of turn, budget kill-switch, interrupt, loop
        // guard) or an error, never a count of laps.
        loop {
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
                Transition::Interrupted(InterruptReason::Cancelled)
            } else if let Some(reason) = pending_abort.take() {
                // A tool refused with "stop the turn": its result is already in
                // the transcript, so the stop happens at this boundary like any
                // other, and reconciliation still runs for the calls that never
                // got one.
                Transition::Interrupted(reason)
            } else if turn.requires_profile_compaction() && pending.is_none() {
                Transition::Compact(CompactKind::Auto)
            } else if force_compact && pending.is_none() {
                // US-030 MidTurn: compaction forced by a long tool_result on the
                // previous turn. Withholding (`pending`) stays PRIORITY: if a
                // context error is waiting, we let `pre_stream_transition` handle
                // it (Recover) and the force stays armed for the turn after.
                force_compact = false;
                Transition::Compact(CompactKind::Auto)
            } else {
                match pre_stream_transition(pending, turn.budget.should_autocompact()) {
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
                            turn.tool_plan = StepToolPlan::capture(dispatch, turn.parallel_tools);
                            context = frame.context;
                            turn.recompute_static_input(&context, &ephemeral_messages, &deps);
                            turn.observe_estimated(&messages, &deps);
                        }
                    }

                    // structural (cheap) microcompaction under light pressure.
                    // PURELY IN MEMORY: it truncates the content of old
                    // tool_results (the append-only log keeps the full history;
                    // resume will restore more context, never less). So we do
                    // NOT write a boundary (otherwise the clear-on-boundary
                    // resume would wrongly wipe the transcript).
                    if turn.budget.should_microcompact() {
                        let pruned = microcompact(&mut messages, config.micro_keep_recent);
                        if pruned > 0 {
                            compaction.record_success();
                            turn.observe_estimated(&messages, &deps);
                            yield AgentEvent::Compacted(CompactKind::Micro);
                        }
                    }

                    let model_switch_from = context_baseline.as_ref().filter(|previous| {
                        turn.model_runtime.as_ref().is_some_and(|runtime| {
                            previous.profile_fingerprint != runtime.fingerprint
                        })
                    });
                    if model_switch_from.is_some()
                        && invalidate_reasoning(&mut messages)
                        && let Err(error) = deps.session.redact_encrypted_reasoning().await
                    {
                        yield AgentEvent::Error(AgentError::Session(error.to_string()));
                        return;
                    }
                    let snapshot = match PromptSnapshot::capture(
                        turn.model.clone(),
                        turn.model_runtime.clone(),
                        turn.reasoning_effort.clone(),
                        turn.system.clone(),
                        context.clone(),
                        &messages,
                        ephemeral_messages.clone(),
                        turn.tool_plan.clone(),
                        config.max_output_tokens,
                        turn.reasoning_replay,
                        model_switch_from,
                    ) {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            yield AgentEvent::Error(AgentError::InvalidRequest(error.to_string()));
                            return;
                        }
                    };
                    for diagnostic in snapshot.diagnostics() {
                        tracing::warn!(
                            target: "pyxis::prompt",
                            prompt_fingerprint = snapshot.fingerprint(),
                            "{diagnostic}"
                        );
                    }
                    turn.static_input_tokens =
                        snapshot.static_input_tokens(deps.tokenizer.as_ref());
                    // The kill-switch consumes the immutable snapshot's bounded
                    // static input, not the live source values used to build it.
                    if usage_budget.is_active() {
                        let estimated_input =
                            estimate_current_input(&messages, turn.static_input_tokens, &deps)
                                as u64;
                        if let Some(reason) = usage_budget
                            .would_exceed(estimated_input, config.max_output_tokens as u64)
                        {
                            yield AgentEvent::Exhausted(reason);
                            return;
                        }
                    }
                    turn.budget.begin_turn();
                    // The figures a reader outside the loop can act on: the
                    // request about to leave carries exactly this much context.
                    // Published here rather than at each budget mutation, so a
                    // tool called during the turn reads the state that turn was
                    // built from instead of a mid-flight intermediate.
                    deps.context_window.publish((&turn.budget).into());
                    active_tool_plan = snapshot.tool_plan().clone();
                    let next_baseline = snapshot.baseline().clone();
                    if let Some(context_transition) = transition_between(
                        context_baseline.as_ref(),
                        &next_baseline,
                        std::mem::take(&mut turn.transition_causes),
                    ) {
                        if let Err(error) = deps
                            .session
                            .record_context_transition(context_transition)
                            .await
                        {
                            yield AgentEvent::Error(AgentError::Session(error.to_string()));
                            return;
                        }
                        context_baseline = Some(next_baseline);
                    }
                    tracing::debug!(
                        target: "pyxis::prompt",
                        prompt_fingerprint = snapshot.fingerprint(),
                        stable_prefix_fingerprint = snapshot.stable_prefix_fingerprint(),
                        turn.reasoning_replay = snapshot.reasoning_replay(),
                        "prompt snapshot opened"
                    );
                    let mut req = snapshot.request();
                    if let Some(turn_id) = turn_id.as_deref() {
                        req.client_metadata
                            .insert(TURN_ID_METADATA_KEY.to_string(), turn_id.to_string());
                    }
                    if let Err(e) = req.validate() {
                        yield AgentEvent::Error(AgentError::InvalidRequest(e.to_string()));
                        return;
                    }
                    let attempt_context = AttemptContext {
                        turn_id: turn_id.clone(),
                        step: model_turns.saturating_add(1),
                        prompt_fingerprint: snapshot.fingerprint().to_string(),
                        model_runtime_fingerprint: snapshot
                            .baseline()
                            .profile_fingerprint
                            .clone(),
                        tool_plan_fingerprint: snapshot
                            .baseline()
                            .tool_plan_fingerprint
                            .clone(),
                    };

                    // US-001: opening the stream can block for several seconds
                    // (TLS + first byte); cancellation takes over without waiting.
                    let opened = match guard(&deps.cancel, deps.provider.stream(req)).await {
                        Cancellable::Cancelled => continue,
                        Cancellable::Completed(opened) => opened,
                    };
                    // Stream consumption: live yields (never ANSI).
                    let mut acc = Accumulator::new();
                    let mut last_usage: Option<TokenUsage> = None;
                    let mut estimated_input: Option<u32> = None;
                    let mut interrupted = false;
                    let mut steered = false;
                    // A sampling fails either when opening or while draining. Both
                    // observations reach the SAME decision below, so all that is kept
                    // here is the error itself, never which of the two produced it.
                    let mut failure: Option<ProviderError> = None;
                    let mut stream = match opened {
                        Ok(stream) => Some(stream),
                        Err(error) => {
                            failure = Some(error);
                            None
                        }
                    };
                    if let Some(stream) = stream.as_mut() {
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
                                                .saturating_add(turn.static_input_tokens),
                                        );
                                    }
                                    turn.budget.observe_usage(usage);
                                    // Real backend usage supersedes the pre-turn
                                    // estimate: republished so the tools of THIS
                                    // turn stop reading an estimate once the
                                    // authoritative count is known.
                                    deps.context_window.publish((&turn.budget).into());
                                    run_usage.add_assign(&usage);
                                    last_usage = Some(usage);
                                }
                                Ok(StreamEvent::Quota { snapshot }) => {
                                    if !snapshot.is_empty() {
                                        yield AgentEvent::Quota(snapshot);
                                    }
                                }
                                Ok(StreamEvent::ReasoningReplayDisabled { reason }) => {
                                    turn.reasoning_replay_downgraded = true;
                                    turn.reasoning_replay = false;
                                    yield AgentEvent::ReasoningReplayDisabled { reason };
                                }
                                Ok(StreamEvent::ResponseMetadata { metadata }) => {
                                    if !metadata.is_empty() {
                                        yield AgentEvent::ResponseMetadata(metadata);
                                    }
                                }
                                Ok(StreamEvent::ResponseItem {
                                    phase,
                                    output_index,
                                    item,
                                }) => {
                                    yield AgentEvent::ResponseItem {
                                        phase,
                                        output_index,
                                        item,
                                    };
                                }
                                Ok(StreamEvent::ProviderExtension { extension }) => {
                                    yield AgentEvent::ProviderExtension(extension);
                                }
                                Ok(StreamEvent::UnmappedItem {
                                    item_type,
                                    extension,
                                }) => {
                                    // The turn continues: an item we cannot read is a
                                    // gap in what the client can show, never a reason
                                    // to fail a turn the model completed.
                                    yield AgentEvent::UnmappedResponseItem {
                                        item_type,
                                        extension,
                                    };
                                }
                                Ok(other) => {
                                    if let Err(e) = acc.push(other) {
                                        yield AgentEvent::Error(e);
                                        return;
                                    }
                                }
                                Err(e) => {
                                    failure = Some(e);
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
                        if failure.is_some() {
                            // The attempt is lost, its tokens are not: they were spent
                            // whether or not the answer ever reached us.
                            record_attempt_usage(
                                &mut usage_budget,
                                &mut turn.budget,
                                last_usage,
                                &messages,
                                turn.static_input_tokens,
                                &acc,
                                &deps,
                            );
                        }
                    }

                    if let Some(error) = failure {
                        // The partial deltas were never committed: the client is told to
                        // erase what it displayed before anything else is emitted.
                        if acc.has_visible_output() {
                            yield AgentEvent::StreamReset;
                        }
                        let (class, action) = turn.plan_failure(
                            &error,
                            &mut config,
                            &context,
                            &ephemeral_messages,
                            &messages,
                            &deps,
                        );
                        match action {
                            FailureAction::Fail(error) => {
                                yield AgentEvent::Error(error);
                                return;
                            }
                            FailureAction::Withhold(kind) => {
                                turn.attempt_ordinal += 1;
                                yield attempt_context.retry_scheduled(
                                    turn.attempt_ordinal,
                                    turn.attempt_policy,
                                    class,
                                    Duration::ZERO,
                                    None,
                                );
                                pending = Some(PendingError { kind });
                                continue;
                            }
                            FailureAction::DowngradeReplay => {
                                turn.attempt_ordinal += 1;
                                yield AgentEvent::ReasoningReplayDisabled {
                                    reason: "backend rejected encrypted reasoning replay".into(),
                                };
                                yield attempt_context.retry_scheduled(
                                    turn.attempt_ordinal,
                                    turn.attempt_policy,
                                    class,
                                    Duration::ZERO,
                                    None,
                                );
                                continue;
                            }
                            FailureAction::RefreshCredentials => {
                                yield attempt_context.credential_refresh(
                                    turn.attempt_ordinal,
                                    CredentialRefreshOutcome::Started,
                                );
                                match guard(&deps.cancel, deps.provider.refresh_auth()).await {
                                    Cancellable::Cancelled => {
                                        yield attempt_context.credential_refresh(
                                            turn.attempt_ordinal,
                                            CredentialRefreshOutcome::Cancelled,
                                        );
                                        continue;
                                    }
                                    Cancellable::Completed(Ok(())) => {
                                        yield attempt_context.credential_refresh(
                                            turn.attempt_ordinal,
                                            CredentialRefreshOutcome::Succeeded,
                                        );
                                    }
                                    Cancellable::Completed(Err(error)) => {
                                        let (outcome, error) = recovery_failure(&error);
                                        yield attempt_context
                                            .credential_refresh(turn.attempt_ordinal, outcome);
                                        yield AgentEvent::Error(AgentError::Auth(error));
                                        return;
                                    }
                                }
                                turn.attempt_ordinal += 1;
                                yield attempt_context.retry_scheduled(
                                    turn.attempt_ordinal,
                                    turn.attempt_policy,
                                    class,
                                    Duration::ZERO,
                                    None,
                                );
                                continue;
                            }
                            FailureAction::Retry { delay, fallback_model } => {
                                turn.attempt_ordinal += 1;
                                yield attempt_context.retry_scheduled(
                                    turn.attempt_ordinal,
                                    turn.attempt_policy,
                                    class,
                                    delay,
                                    fallback_model,
                                );
                                if !delay.is_zero() {
                                    let _ = guard(&deps.cancel, deps.clock.sleep(delay)).await;
                                }
                                continue;
                            }
                        }
                    }

                    turn.attempt_ordinal = 1;
                    turn.credential_refresh_attempted = false;
                    turn.attempt_policy = ResolvedAttemptPolicy::resolve(&config, turn.model_runtime.as_ref());
                    model_turns += 1;

                    // Usage fallback: without an `usage` in the stream, estimate
                    // locally to feed the compaction threshold (invariant 7). We also
                    // account the turn in the US-014 budget (real when available,
                    // otherwise estimated: context input + generated output).
                    record_attempt_usage(
                        &mut usage_budget,
                        &mut turn.budget,
                        last_usage,
                        &messages,
                        turn.static_input_tokens,
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
                        last_usage,
                        total_usage: run_usage,
                        // US-002: real occupancy of the window, absent when the
                        // provider reported nothing (never reported as zero).
                        context_tokens: last_usage.map(|usage| usage.input),
                        context_window: turn.model_runtime
                            .as_ref()
                            .map(|runtime| runtime.context_window)
                            .or_else(|| deps.provider.context_window_for_model(&turn.model)),
                        auto_compact_token_limit: turn.model_runtime
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

                    // US-014 / ADR-14: deterministic loop guardrail (FR-05). It
                    // OVERRIDES the model's logic. From the first tier of
                    // LOOP_GUARD_THRESHOLDS on, the batch is never executed; what
                    // escalates is the register of the answer, and the last tier
                    // stops the run (iter_cap stays the last resort).
                    let loop_decision = match guarded_batch_signature(
                        &calls,
                        active_tool_plan.dispatcher().as_ref(),
                    ) {
                        Some(signature) => loop_guard.observe(signature),
                        None => {
                            loop_guard.reset();
                            LoopDecision::Proceed
                        }
                    };
                    match loop_decision {
                        LoopDecision::Abort => {
                            // One terminal state per turn (invariant 11): the whole
                            // batch dies on this single event, however many calls it
                            // carried.
                            yield AgentEvent::Exhausted(ExhaustReason::ToolLoop {
                                count: loop_guard.count(),
                            });
                            return;
                        }
                        LoopDecision::Signal(register) => {
                            // Hard stop on the repeated batch: we DO NOT EXECUTE, we send
                            // an explicit signal back to the agent (edge case #2). One
                            // tool_result per tool_use -> valid transcript.
                            for c in &calls {
                                let msg = loop_guard_message(
                                    register,
                                    &c.name,
                                    loop_guard.count(),
                                    &c.input,
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
                                    kind: turn.tool_plan.dispatcher().call_kind(c),
                                });
                            }
                            let (tool_event_tx, mut tool_event_rx) =
                                tokio::sync::mpsc::unbounded_channel();
                            let expected_ids: Vec<ToolCallId> =
                                calls.iter().map(|c| c.id.clone()).collect();
                            let dispatch = active_tool_plan
                                .dispatch(calls, ToolEventSink::new(tool_event_tx));
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
                                            Some(event) => yield AgentEvent::from(event),
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
                                feedback_limits(&turn.model_runtime);
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
                                yield AgentEvent::from(event);
                            }
                            if let Err(e) = validate_tool_outcomes(&expected_ids, &outcomes) {
                                yield AgentEvent::Error(e);
                                return;
                            }
                            for o in &outcomes {
                                if o.aborts_turn {
                                    pending_abort = Some(InterruptReason::ToolAborted);
                                }
                                yield AgentEvent::ToolResult(ToolResultView::from_model(o));
                                // US-011: images ride INSIDE the result now. They
                                // used to enter as a user message right after it,
                                // which put a turn the user never took between two
                                // tool turns. The full compaction still drops them
                                // (`compaction::strip_for_summary`): vision is not
                                // paid for twice.
                                messages.push(Message::tool_result_from_model(o));
                            }
                            // US-030 MidTurn: the tool_results we just added are NOT
                            // in the budget yet (it is based on the previous turn's
                            // usage). We PROJECT their weight (without overwriting the
                            // real budget); if a long result crosses the threshold, we
                            // force compaction on the next turn, before the model.
                            let projected = estimate_current_input(&messages, turn.static_input_tokens, &deps);
                            if turn.budget.would_autocompact(projected) {
                                force_compact = true;
                            }
                            // A tool that asked for a fresh window rides the SAME
                            // arming: the request is granted as a compaction at the
                            // next safe point, never as an immediate transcript
                            // rewrite between a `tool_use` and its result.
                            if outcomes.iter().any(|o| o.requests_compaction) {
                                force_compact = true;
                            }
                            // loop back: the model sees the results.
                        }
                    }
                }
                Transition::Compact(kind) => {
                    let compact_model = turn.compaction_model().to_string();
                    // US-001: compaction is a full model call; cancellation does not
                    // wait for it. The transcript is not modified until
                    // `full_compact` has handed back control.
                    let compacted = match guard(
                        &deps.cancel,
                        full_compact(
                            &mut messages,
                            &compact_model,
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
                            if turn.required_profile_compaction.take().is_none() {
                                context_baseline = None;
                                turn.transition_causes.push(ContextTransitionCause::Compaction);
                            }
                            // US-030: anchors the baseline on the NEXT real usage
                            // (guards against an immediate double compaction).
                            let compacted_input = estimate_current_input(&messages, turn.static_input_tokens, &deps);
                            turn.budget.mark_compacted(compacted_input);
                            yield AgentEvent::Compacted(kind);
                        }
                        Err(_) => {
                            if turn.required_profile_compaction.is_some() {
                                yield AgentEvent::Error(AgentError::Provider(
                                    ProviderFailure::contract(
                                        "required comp_hash compaction failed before model switch",
                                    ),
                                ));
                                return;
                            }
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
                            turn.budget.observe_estimated(estimate_current_input(&messages, turn.static_input_tokens, &deps));
                        }
                    }
                }
                Transition::Recover(_) => {
                    // withholding: REACTIVE compaction; confirmed failure -> propagation.
                    let compacted = match guard(
                        &deps.cancel,
                        full_compact(
                            &mut messages,
                            &turn.model,
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
                            context_baseline = None;
                            turn.transition_causes.push(ContextTransitionCause::Compaction);
                            let compacted_input = estimate_current_input(&messages, turn.static_input_tokens, &deps);
                            turn.budget.mark_compacted(compacted_input);
                            yield AgentEvent::Compacted(CompactKind::Reactive);
                        }
                        Err(e) => {
                            yield AgentEvent::Error(AgentError::ContextUnrecoverable(e.to_string()));
                            return;
                        }
                    }
                }
                Transition::Interrupted(reason) => {
                    // US-002: reconciliation BEFORE persistence, every call left
                    // unanswered gets an explicit result, otherwise the next turn
                    // re-emits an orphan `function_call` that the backend rejects.
                    let mut reconciled_tool_calls = 0u32;
                    for view in reconcile_interrupted_calls(&mut messages) {
                        reconciled_tool_calls = reconciled_tool_calls.saturating_add(1);
                        yield AgentEvent::ToolResult(view);
                    }
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }
                    // US-001: the event is emitted by the CORE, the client no longer
                    // has to build it after an abort decided from the outside.
                    let completed_at_ms = deps.clock.now_ms();
                    yield AgentEvent::Interrupted(crate::event::InterruptedView {
                        reason,
                        started_at_ms: Some(run_started_ms),
                        completed_at_ms: Some(completed_at_ms),
                        duration_ms: Some(completed_at_ms.saturating_sub(run_started_ms)),
                        reconciled_tool_calls,
                    });
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
        let policy = ResolvedAttemptPolicy::resolve(&cfg, None);
        let err = ProviderError::Http {
            status: 529,
            message: String::new(),
            retry_after_ms: None,
        };
        let overloaded = transient_retry_delay(policy, 0, ErrorClass::Overloaded(529), &err, 0);
        let retryable = transient_retry_delay(policy, 0, ErrorClass::Retryable, &err, 0);
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
        let policy = ResolvedAttemptPolicy::resolve(&cfg, None);
        let err = http(Some(3_600_000));
        assert_eq!(
            transient_retry_delay(policy, 0, ErrorClass::RateLimited, &err, 0),
            Duration::from_millis(MAX_RETRY_AFTER_MS)
        );
    }

    // backoff: exponential capped at 32x (2^5), no overflow.
    #[test]
    fn backoff_is_exponential_capped() {
        let cfg = RunConfig {
            backoff_base_ms: 10,
            ..RunConfig::default()
        };
        let policy = ResolvedAttemptPolicy::resolve(&cfg, None);
        assert_eq!(backoff(policy, 0), Duration::from_millis(10));
        assert_eq!(backoff(policy, 1), Duration::from_millis(20));
        assert_eq!(backoff(policy, 2), Duration::from_millis(40));
        // past 2^5 the factor is pinned at 32.
        assert_eq!(backoff(policy, 5), Duration::from_millis(320));
        assert_eq!(backoff(policy, 50), Duration::from_millis(320));
    }
}
