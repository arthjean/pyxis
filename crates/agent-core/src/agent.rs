//! `run_agent` — la boucle d'agent : state machine à transitions typées, exposée
//! comme un `Stream<AgentEvent>` (async-stream). Headless : elle ne pousse rien
//! vers un terminal, elle yield des événements structurés (jamais d'ANSI).
//!
//! Implémente : transcript-before-response (invariant 6), withholding
//! (PendingError de contexte, invariant 8), compaction en cascade (§5), retry
//! transverse des erreurs transitoires (≠ withholding), et le `match` exhaustif
//! sur `Transition` (AC1).

use std::time::Duration;

use futures_util::{Stream, StreamExt};

use crate::budget::{ContextBudget, estimate_input, estimate_static_input};
use crate::compaction::{CompactKind, CompactionState, full_compact, microcompact};
use crate::deps::Deps;
use crate::error::{AgentError, ProviderFailure};
use crate::event::{AgentEvent, ToolCallView, ToolResultView};
use crate::guardrail::{CostBudget, LoopDecision, LoopGuard, UsageBudget, batch_signature};
use crate::message::{Message, ToolCallId};
use crate::provider::{
    AuthError, CanonicalRequest, ErrorClass, ProviderError, StreamEvent, TokenUsage, ToolSpec,
};
use crate::tools::{ToolDispatchEvent, ToolEventSink, ToolOutcome};
use crate::transition::{
    Accumulator, ContextErrorKind, ExhaustReason, PendingError, Transition, post_stream_transition,
    pre_stream_transition,
};

/// Réglages de la boucle (garde-fous, seuils).
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub max_turns: u32,
    pub max_output_tokens: u32,
    pub max_retries: u32,
    pub micro_keep_recent: usize,
    pub compaction_breaker_limit: u32,
    pub backoff_base_ms: u64,
    /// US-014 — répétitions identiques de batch d'outils avant signal de boucle
    /// (défaut 3). Au-delà du signal → arrêt déterministe.
    pub loop_guard_threshold: u32,
    /// US-014 — budget cumulé de tokens (kill-switch). `None` = désactivé.
    pub token_budget: Option<u64>,
    /// US-014 — budget cumulé de coût (kill-switch). `None` = désactivé.
    pub cost_budget: Option<CostBudget>,
    /// Modèle de repli optionnel après une surcharge provider.
    pub overload_fallback_model: Option<String>,
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
        }
    }
}

/// Contexte d'une exécution d'agent (modèle, system, transcript, outils).
pub struct AgentContext {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub config: RunConfig,
    /// Messages de contexte ÉPHÉMÈRES (US-028) : AGENTS.md + bloc environnement,
    /// préfixés à CHAQUE requête mais JAMAIS poussés dans `messages` ni persistés
    /// (rechargés par tour, pas accumulés). Stateless-safe : le contexte projet est
    /// re-fourni à chaque tour sans polluer le transcript ni `instructions`.
    pub context_messages: Vec<Message>,
    /// Messages de contrôle éphémères ajoutés après le transcript pour la requête
    /// courante, sans persistance. Exemple: relance automatique d'un objectif.
    pub ephemeral_messages: Vec<Message>,
}

impl AgentContext {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            config: RunConfig::default(),
            context_messages: Vec::new(),
            ephemeral_messages: Vec::new(),
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
}

fn make_request(
    model: &str,
    system: &Option<String>,
    context_messages: &[Message],
    messages: &[Message],
    ephemeral_messages: &[Message],
    tools: &[ToolSpec],
    max_output: u32,
) -> CanonicalRequest {
    // US-028 : préfixe ÉPHÉMÈRE (AGENTS.md + env). Stable avant volatil pour
    // préserver le préfixe cacheable ; jamais persisté (le transcript reste
    // `messages` seul).
    let mut all =
        Vec::with_capacity(context_messages.len() + messages.len() + ephemeral_messages.len());
    all.extend_from_slice(context_messages);
    all.extend_from_slice(messages);
    all.extend_from_slice(ephemeral_messages);
    CanonicalRequest {
        model: model.to_string(),
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

/// Plafond du délai `Retry-After` honoré (US-023). Un serveur ne peut pas geler la
/// boucle indéfiniment : un délai aberrant est borné, on retente puis on abandonne
/// selon `max_retries`. Identique au cap de Pi (60 s).
const MAX_RETRY_AFTER_MS: u64 = 60_000;

/// Délai de retry effectif (US-023) : `max(backoff exponentiel, Retry-After)`, le
/// délai serveur (ms exact) primant quand il est plus long, borné à
/// `MAX_RETRY_AFTER_MS`. Les erreurs sans en-tête serveur retombent sur le backoff.
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

/// Lance l'agent. Renvoie un `Stream<AgentEvent>` à consommer (TUI, `-p`, Paneflow).
pub fn run_agent(ctx: AgentContext, deps: Deps) -> impl Stream<Item = AgentEvent> + Send {
    async_stream::stream! {
        let AgentContext {
            mut model,
            system,
            mut messages,
            tools,
            config,
            context_messages,
            ephemeral_messages,
        } = ctx;

        // ContextBudget calculé pour le modèle actif (recalculé si fallback overload).
        let max_context = deps.provider.max_context_for_model(&model);
        let mut budget = match ContextBudget::try_for_model(max_context, config.max_output_tokens) {
            Ok(budget) => budget,
            Err(e) => {
                yield AgentEvent::Error(AgentError::InvalidRequest(e));
                return;
            }
        };
        // L'usage backend compte tout ce qui est envoyé : system, contexte
        // éphémère, schémas d'outils et transcript. Les projections locales doivent
        // porter le même overhead statique, sinon la compaction arrive trop tard.
        let static_input_tokens = estimate_static_input(
            &system,
            &context_messages,
            &tools,
            deps.tokenizer.as_ref(),
        )
        .saturating_add(estimate_input(&ephemeral_messages, deps.tokenizer.as_ref()));
        let mut compaction = CompactionState::default();
        let mut pending: Option<PendingError> = None;
        let mut model_turns: u32 = 0;
        let mut transient_retries: u32 = 0;
        let mut overload_fallback_used = false;
        let mut iterations: u32 = 0;
        let iter_cap = config.max_turns.saturating_mul(4).saturating_add(32);
        // US-014 — garde-fous déterministes (override de la logique du modèle).
        let mut loop_guard = LoopGuard::new(config.loop_guard_threshold);
        let mut usage_budget = UsageBudget::new(config.token_budget, config.cost_budget);
        // US-030 (MidTurn) : armé quand un long tool_result franchit le seuil →
        // force la compaction au prochain tour, AVANT de relancer le modèle.
        let mut force_compact = false;

        loop {
            iterations += 1;
            if iterations > iter_cap {
                yield AgentEvent::Error(AgentError::Provider(ProviderFailure::contract(
                    "garde-fou d'itérations atteint",
                )));
                return;
            }

            // transcript-before-response (invariant 6) — delta idempotent.
            if let Err(e) = deps.session.sync(&messages).await {
                yield AgentEvent::Error(AgentError::Session(e.to_string()));
                return;
            }

            // US-014 — kill-switch budget : seuil cumulé atteint → arrêt (edge
            // case #3). L'estimation PRÉ-tour est faite plus bas, avant le stream.
            if let Some(reason) = usage_budget.exceeded() {
                yield AgentEvent::Exhausted(reason);
                return;
            }

            let transition: Transition = if force_compact && pending.is_none() {
                // US-030 MidTurn : compaction forcée par un long tool_result au tour
                // précédent. Le withholding (`pending`) reste PRIORITAIRE : si une
                // erreur de contexte est en attente, on laisse `pre_stream_transition`
                // la traiter (Recover) et le force reste armé pour le tour d'après.
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
                    // microcompaction structurelle (cheap) sous pression légère.
                    // PUREMENT EN MÉMOIRE : elle tronque le contenu de vieux
                    // tool_results (le log append-only garde l'historique complet ;
                    // le resume restaurera plus de contexte, jamais moins). On
                    // n'écrit donc PAS de frontière (sinon le resume clear-on-
                    // boundary effacerait le transcript à tort).
                    if budget.should_microcompact() {
                        let pruned = microcompact(&mut messages, config.micro_keep_recent);
                        if pruned > 0 {
                            compaction.record_success();
                            budget.observe_estimated(estimate_current_input(&messages, static_input_tokens, &deps));
                            yield AgentEvent::Compacted(CompactKind::Micro);
                        }
                    }

                    // US-014 — estimation pré-tour : stoppe AVANT un tour dont la
                    // projection (contexte estimé + sortie max) franchirait le
                    // budget (edge case #3, « avant un gros tour »).
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
                        &system,
                        &context_messages,
                        &messages,
                        &ephemeral_messages,
                        &tools,
                        config.max_output_tokens,
                    );
                    if let Err(e) = req.validate() {
                        yield AgentEvent::Error(AgentError::InvalidRequest(e.to_string()));
                        return;
                    }

                    let mut stream = match deps.provider.stream(req).await {
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
                                // attempt indexé à partir de 0 → délais 1×,2×,4×.
                                // US-023 : honore Retry-After (max(backoff, retry_after), borné).
                                deps.clock
                                    .sleep(transient_retry_delay(
                                        &config,
                                        transient_retries - 1,
                                        class,
                                        &e,
                                        deps.clock.now_ms(),
                                    ))
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

                    // Consommation du stream : yields live (jamais d'ANSI).
                    let mut acc = Accumulator::new();
                    let mut stream_err: Option<ProviderError> = None;
                    let mut last_usage: Option<TokenUsage> = None;
                    while let Some(ev) = stream.next().await {
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
                                // Sonde d'observabilité (US-021 AC3 / US-029) : compare
                                // l'usage backend réel à l'estimation locale. Env-gated,
                                // défaut OFF → chemin et sortie inchangés en prod.
                                if std::env::var_os("PYXIS_DEBUG_USAGE").is_some() {
                                    let est_in = estimate_input(&messages, deps.tokenizer.as_ref())
                                        .saturating_add(static_input_tokens);
                                    eprintln!(
                                        "[usage] backend input={} output={} | estimé_local input≈{} (ratio réel/estimé={:.3})",
                                        usage.input,
                                        usage.output,
                                        est_in,
                                        usage.input as f64 / (est_in.max(1) as f64),
                                    );
                                }
                                budget.observe_usage(usage);
                                last_usage = Some(usage);
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
                                // attempt indexé à partir de 0 → délais 1×,2×,4×.
                                // US-023 : honore Retry-After (max(backoff, retry_after), borné).
                                deps.clock
                                    .sleep(transient_retry_delay(
                                        &config,
                                        transient_retries - 1,
                                        class,
                                        &e,
                                        deps.clock.now_ms(),
                                    ))
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

                    // Fallback usage : si pas d'`usage` en stream, estime
                    // localement pour alimenter le seuil de compaction (invariant 7). On
                    // comptabilise aussi le tour dans le budget US-014 (réel si
                    // disponible, sinon estimé : input contexte + output généré).
                    record_attempt_usage(
                        &mut usage_budget,
                        &mut budget,
                        last_usage,
                        &messages,
                        static_input_tokens,
                        &acc,
                        &deps,
                    );

                    let transition = post_stream_transition(&acc);
                    let commits_assistant =
                        matches!(transition, Transition::EndTurn | Transition::RunTools(_));
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

            // Match EXHAUSTIF sur les 6 variantes (AC1) — vérifié à la compilation.
            match transition {
                Transition::EndTurn => {
                    // US-024 — persistance du DERNIER tour assistant : le message
                    // assistant final (acc.to_assistant_message) vient d'être poussé,
                    // mais le sync d'en-tête de boucle ne s'exécuterait qu'au tour
                    // SUIVANT, qui n'aura pas lieu. Sync final (delta-only, idempotent)
                    // avant de rendre la main, sinon `/resume` perd la dernière réponse.
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }
                    yield AgentEvent::EndTurn;
                    return;
                }
                Transition::RunTools(calls) => {
                    // transcript-before-response pour le TOUR ASSISTANT : le message
                    // assistant (avec ses tool_use, déjà pushé) est persisté AVANT
                    // d'exécuter les outils. Sinon un crash pendant le dispatch
                    // laisserait des tool_results orphelins (sans tour assistant) au
                    // resume — transcript structurellement invalide (#1).
                    if let Err(e) = deps.session.sync(&messages).await {
                        yield AgentEvent::Error(AgentError::Session(e.to_string()));
                        return;
                    }

                    // US-014 — garde-fou de boucle déterministe (FR-05) : il OVERRIDE
                    // la logique du modèle. Au seuil → signal sans exécuter ;
                    // au-delà → arrêt déterministe (l'iter_cap reste le filet ultime).
                    match loop_guard.observe(batch_signature(&calls)) {
                        LoopDecision::Abort => {
                            yield AgentEvent::Exhausted(ExhaustReason::ToolLoop {
                                count: loop_guard.count(),
                            });
                            return;
                        }
                        LoopDecision::Signal => {
                            // Hard stop du batch répété : on N'EXÉCUTE PAS, on renvoie
                            // un signal explicite à l'agent (edge case #2). Un
                            // tool_result par tool_use → transcript valide.
                            for c in &calls {
                                let msg = format!(
                                    "Boucle détectée sur {} (×{}) — arrêt. Reformulez l'approche \
                                     ou demandez une intervention.",
                                    c.name,
                                    loop_guard.count(),
                                );
                                yield AgentEvent::ToolResult(ToolResultView {
                                    id: c.id.clone(),
                                    content: msg.clone(),
                                    is_error: true,
                                    error_kind: Some(crate::message::ToolErrorKind::Semantic),
                                    untrusted: false,
                                });
                                messages.push(Message::tool_result_with_metadata(
                                    c.id.clone(),
                                    msg,
                                    true,
                                    false,
                                    Some(crate::message::ToolErrorKind::Semantic),
                                ));
                            }
                            // reboucle : le modèle reçoit le signal et peut corriger.
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
                                deps.tools.dispatch(calls, ToolEventSink::new(tool_event_tx));
                            tokio::pin!(dispatch);
                            let mut tool_events_open = true;
                            let outcomes = loop {
                                tokio::select! {
                                    event = tool_event_rx.recv(), if tool_events_open => {
                                        match event {
                                            Some(ToolDispatchEvent::PermissionAsk(req)) => {
                                                yield AgentEvent::PermissionAsk(req);
                                            }
                                            None => tool_events_open = false,
                                        }
                                    }
                                    outcomes = &mut dispatch => break outcomes,
                                }
                            };
                            while let Ok(event) = tool_event_rx.try_recv() {
                                match event {
                                    ToolDispatchEvent::PermissionAsk(req) => {
                                        yield AgentEvent::PermissionAsk(req);
                                    }
                                }
                            }
                            if let Err(e) = validate_tool_outcomes(&expected_ids, &outcomes) {
                                yield AgentEvent::Error(e);
                                return;
                            }
                            for o in &outcomes {
                                yield AgentEvent::ToolResult(ToolResultView {
                                    id: o.id.clone(),
                                    content: o.content.clone(),
                                    is_error: o.is_error,
                                    error_kind: o.error_kind,
                                    untrusted: o.untrusted,
                                });
                                messages.push(Message::tool_result_with_metadata(
                                    o.id.clone(),
                                    o.content.clone(),
                                    o.is_error,
                                    o.untrusted,
                                    o.error_kind,
                                ));
                            }
                            // US-030 MidTurn : les tool_results qu'on vient d'ajouter
                            // ne sont PAS encore dans le budget (basé sur l'usage du
                            // tour précédent). On PROJETTE leur poids (sans écraser le
                            // budget réel) ; si un long résultat franchit le seuil, on
                            // force la compaction au prochain tour, avant le modèle.
                            let projected = estimate_current_input(&messages, static_input_tokens, &deps);
                            if budget.would_autocompact(projected) {
                                force_compact = true;
                            }
                            // reboucle : le modèle voit les résultats.
                        }
                    }
                }
                Transition::Compact(kind) => {
                    match full_compact(
                        &mut messages,
                        &model,
                        deps.provider.as_ref(),
                        config.max_output_tokens,
                    )
                    .await
                    {
                        Ok(usage) => {
                            usage_budget.record_usage(usage);
                            compaction.record_success();
                            // checkpoint ATOMIQUE : frontière + transcript résumé en
                            // une opération ; erreur I/O propagée (pas de let _ qui
                            // désynchroniserait le curseur de session — #8).
                            if let Err(e) = deps.session.checkpoint(kind, &messages).await {
                                yield AgentEvent::Error(AgentError::Session(e.to_string()));
                                return;
                            }
                            // US-030 : ancre le baseline sur le PROCHAIN usage réel
                            // (anti double-compaction immédiate).
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
                            // anti error-loop : microcompact structurel pour baisser
                            // la pression avant de reboucler.
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
                    // withholding : compaction REACTIVE ; échec confirmé → propagation.
                    match full_compact(
                        &mut messages,
                        &model,
                        deps.provider.as_ref(),
                        config.max_output_tokens,
                    )
                    .await
                    {
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

// ───────────────────────── Mode headless (`-p`) ─────────────────────────

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

/// Consomme la boucle en mode headless : agrège le texte, AUCUN Ratatui (AC3).
/// C'est ce que `pyxis -p` câblera (agent-cli) ; ici, testable sans terminal.
pub async fn run_headless(ctx: AgentContext, deps: Deps) -> HeadlessResult {
    let stream = run_agent(ctx, deps);
    futures_util::pin_mut!(stream);

    let mut text = String::new();
    let mut pending_text = String::new();
    let mut events = 0usize;
    let mut ended = HeadlessEnd::EndTurn;

    while let Some(ev) = stream.next().await {
        events += 1;
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

    // US-023 : sans en-tête serveur → backoff seul.
    #[test]
    fn retry_delay_without_header_uses_backoff() {
        let base = Duration::from_millis(50);
        assert_eq!(retry_delay(base, &http(None)), base);
        assert_eq!(
            retry_delay(base, &ProviderError::Transport("x".into())),
            base
        );
    }

    // US-023 : Retry-After plus long que le backoff → c'est lui qui prime.
    #[test]
    fn retry_delay_honors_longer_retry_after() {
        let base = Duration::from_millis(50);
        assert_eq!(
            retry_delay(base, &http(Some(2_000))),
            Duration::from_millis(2_000)
        );
    }

    // US-023 : backoff plus long que Retry-After → le backoff prime (max).
    #[test]
    fn retry_delay_keeps_longer_backoff() {
        let base = Duration::from_millis(5_000);
        assert_eq!(retry_delay(base, &http(Some(1_000))), base);
    }

    // US-023 : un Retry-After aberrant est borné (jamais de gel indéfini).
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

    // backoff : exponentiel plafonné à 32× (2^5), pas de débordement.
    #[test]
    fn backoff_is_exponential_capped() {
        let cfg = RunConfig {
            backoff_base_ms: 10,
            ..RunConfig::default()
        };
        assert_eq!(backoff(&cfg, 0), Duration::from_millis(10));
        assert_eq!(backoff(&cfg, 1), Duration::from_millis(20));
        assert_eq!(backoff(&cfg, 2), Duration::from_millis(40));
        // au-delà de 2^5 le facteur est figé à 32.
        assert_eq!(backoff(&cfg, 5), Duration::from_millis(320));
        assert_eq!(backoff(&cfg, 50), Duration::from_millis(320));
    }
}
