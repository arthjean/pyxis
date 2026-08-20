//! Deterministic guardrails (US-014, EP-003). They OVERRIDE the model's
//! fallible logic, from outside the loop:
//!
//! - `LoopGuard`: detects the same tool batch (same names + same args)
//!   repeated N times in a row, and escalates over the three tiers of
//!   `LOOP_GUARD_THRESHOLDS`. At every tier the batch is NOT executed; what
//!   changes is the register of the message handed back and, at the last tier,
//!   the fact that the run stops.
//! - `UsageBudget`: cumulated token/cost budget with a **kill-switch** at 100% and
//!   a pre-turn estimate to stop *before* a turn that costs too much.
//!
//! These guardrails live in `agent-core` (and not `agent-tools`) because the
//! dependency graph forbids `core -> tools`, and stopping the loop (`Exhausted`)
//! is a termination decision that belongs to the core. Pure, no I/O ->
//! unit-testable.
//!
//! Written up in `docs/ARCHITECTURE.md` §3.6 (« Garde-fous déterministes »), and
//! decided in `docs/DECISIONS.md`, ADR-14: why the offending batch is never
//! executed, why the detection key is never truncated, and what the ladder beat.

use crate::provider::TokenUsage;
use crate::tools::{ToolDispatch, ToolInvocation};
use crate::transition::ExhaustReason;

/// Escalation ladder, in consecutive identical batches. Shared by the outer
/// agent loop and by Code Mode nested dispatch, because two ladders would be a
/// second source of truth for the same decision.
///
/// Below the first tier the batch runs. From the first tier on it never runs
/// again; what escalates is the register of the message the model gets, and the
/// last tier stops the run. An orchestration limit, so a crate constant and not
/// a configuration key (invariant 15).
pub const LOOP_GUARD_THRESHOLDS: [u32; 3] = [3, 5, 8];

/// Byte ceiling on the arguments quoted back by the detailed reminder. Bounds
/// the MESSAGE only: the detection key stays the full canonical string, because
/// truncating it would make two megabyte-sized `write` bodies that differ past
/// the ceiling look like a loop.
pub const LOOP_GUARD_ARGS_PREVIEW_BYTES: usize = 500;

/// Is a ladder usable? Fail-loud rather than a silent fallback to a default:
/// the fallback does not erase the mistake, it moves it downstream.
///
/// Strictly increasing rejects both a decreasing ladder and a duplicate tier. A
/// first tier under 2 is refused because a guard that fires on the first repeat
/// cannot tell a loop from a retry after a transient failure.
pub const fn loop_guard_thresholds_are_valid(thresholds: &[u32; 3]) -> bool {
    thresholds[0] >= 2 && thresholds[0] < thresholds[1] && thresholds[1] < thresholds[2]
}

// Applied to the constant itself: an invalid ladder written in this file fails
// `cargo build`, which is where a Rust crate validates what dsh validates at
// plugin load time.
const _: () = {
    assert!(
        loop_guard_thresholds_are_valid(&LOOP_GUARD_THRESHOLDS),
        "LOOP_GUARD_THRESHOLDS must be strictly increasing and start at 2 or more"
    );
};

/// Register of the message the guard hands back to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopRegister {
    /// First tier: generic and short, quotes nothing, so its cost is constant.
    Gentle,
    /// Second tier: names the tool, the length of the run and the arguments
    /// being repeated, because a model that did not act on the generic reminder
    /// needs to be told what it is repeating.
    Detailed,
    /// Last tier: the run is over. Carried by the message the nested site locks
    /// itself with; the outer site ends the turn instead of answering.
    Terminal,
}

/// Loop guardrail decision for a tool batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    /// No loop: run normally.
    Proceed,
    /// A reminder tier was reached: do not execute, answer in this register.
    /// `observe` never puts `LoopRegister::Terminal` here; that case is `Abort`.
    Signal(LoopRegister),
    /// Last tier: deterministic stop of the loop.
    Abort,
}

/// Builds the English text the model reads. Both call sites go through it, so
/// the ladder speaks with one voice; composing a message locally is what let
/// the nested site say the same thing at two different tiers.
pub fn loop_guard_message(
    register: LoopRegister,
    tool: &str,
    count: u32,
    arguments: &serde_json::Value,
) -> String {
    match register {
        LoopRegister::Gentle => {
            "Loop guard: this tool call repeats the previous one exactly, so it \
             was not executed. Take a different approach before calling again."
                .to_string()
        }
        LoopRegister::Detailed => format!(
            "Loop guard: `{tool}` was called {count} times in a row with identical arguments, and \
             this call was not executed. Arguments: {}. Change the arguments or the approach, or \
             ask the user.",
            preview_arguments(arguments)
        ),
        LoopRegister::Terminal => format!(
            "Loop guard: `{tool}` was called {count} times in a row with identical arguments. \
             Dispatch is stopped for the rest of this turn. Arguments: {}.",
            preview_arguments(arguments)
        ),
    }
}

/// Canonical arguments, cut on a character boundary under
/// `LOOP_GUARD_ARGS_PREVIEW_BYTES`, saying how much was dropped. Never truncate
/// in silence what the model is reasoning about.
fn preview_arguments(arguments: &serde_json::Value) -> String {
    let canonical = arguments.to_string();
    if canonical.len() <= LOOP_GUARD_ARGS_PREVIEW_BYTES {
        return canonical;
    }
    let mut cut = LOOP_GUARD_ARGS_PREVIEW_BYTES;
    while cut > 0 && !canonical.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = canonical.len() - cut;
    format!("{}... [{dropped} more bytes]", &canonical[..cut])
}

/// Tool loop detector (FR-05). Compares the signature of the current batch to
/// the previous one; counts consecutive repeats and reads the tier off the
/// ladder it was built with.
#[derive(Debug)]
pub struct LoopGuard {
    thresholds: [u32; 3],
    last_sig: Option<String>,
    count: u32,
}

impl LoopGuard {
    /// `thresholds` is the escalation ladder, normally `LOOP_GUARD_THRESHOLDS`.
    /// Invalid ladders are a programming error, not a value to repair: a
    /// `debug_assert!` rather than the silent `max(1)` that used to hide one.
    pub fn new(thresholds: [u32; 3]) -> Self {
        debug_assert!(
            loop_guard_thresholds_are_valid(&thresholds),
            "loop guard thresholds must be strictly increasing and start at 2 or more"
        );
        Self {
            thresholds,
            last_sig: None,
            count: 0,
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Breaks the consecutive run. Two callers, and only two: a steering input
    /// entering the transcript, and a fresh turn building a new guard. A batch
    /// the dispatcher declares exempt used to reset here and no longer does
    /// (US-065): it is transparent, because a `wait` between two identical
    /// calls is part of the loop, not a break in it.
    pub fn reset(&mut self) {
        self.last_sig = None;
        self.count = 0;
    }

    /// Folds in the signature of the current batch and decides.
    pub fn observe(&mut self, signature: String) -> LoopDecision {
        if self.last_sig.as_deref() == Some(signature.as_str()) {
            self.count = self.count.saturating_add(1);
        } else {
            self.last_sig = Some(signature);
            self.count = 1;
        }
        // Ranges, not exact counts: a guard that does not execute owes one
        // `tool_result` per `tool_use` at every batch, so it cannot fall silent
        // between two tiers the way a consultative reminder can.
        if self.count < self.thresholds[0] {
            LoopDecision::Proceed
        } else if self.count < self.thresholds[1] {
            LoopDecision::Signal(LoopRegister::Gentle)
        } else if self.count < self.thresholds[2] {
            LoopDecision::Signal(LoopRegister::Detailed)
        } else {
            LoopDecision::Abort
        }
    }
}

/// Signature used by the loop guard.
///
/// Deterministic: `name\0json` per call, joined. The `Display` of
/// `serde_json::Value` produces compact JSON with sorted keys (`serde_json::Map`
/// without `preserve_order`) -> the signature is stable from one turn to the
/// next, and the order of the calls inside the batch does not change it.
///
/// Both halves of that sentence are assumptions about a dependency, so both are
/// proved rather than asserted (US-067):
/// `the_signature_is_blind_to_the_order_of_the_json_keys` for the keys inside
/// one call, `batch_signature_is_order_independent_and_distinct` for the order
/// of the calls. A transitive activation of `preserve_order` fails the first of
/// them instead of breaking detection in silence.
///
/// Calls the dispatcher declares exempt are left out: repeating them is the
/// protocol for making progress, not a symptom of a loop. The core asks rather
/// than guesses, because a name and a JSON value cannot tell an orchestration
/// cell from a tool that merely shares its name. `None` when nothing guardable
/// remains: the call sites then proceed WITHOUT touching the run, so an exempt
/// batch is transparent rather than a reset (US-065).
pub fn guarded_batch_signature(
    calls: &[ToolInvocation],
    dispatch: &dyn ToolDispatch,
) -> Option<String> {
    let mut parts: Vec<String> = calls
        .iter()
        .filter(|call| !dispatch.loop_guard_exempt(call))
        .map(|call| format!("{}\u{0}{}", call.name, call.input))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    Some(parts.join("\u{1}"))
}

/// Pricing of a model, in micro-USD (1e-6 $) per thousand tokens. `u64` to
/// stay `Copy`/`Eq` (no `f64` in the config or in `ExhaustReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostBudget {
    pub limit_micro_usd: u64,
    pub input_micro_per_ktok: u64,
    pub output_micro_per_ktok: u64,
}

/// Cumulated session budget (tokens and/or cost). Disabled by default
/// (`token_limit`/`cost` at `None`) -> no impact on sessions without a budget.
#[derive(Debug, Clone, Default)]
pub struct UsageBudget {
    token_limit: Option<u64>,
    cost: Option<CostBudget>,
    spent_input: u64,
    spent_output: u64,
}

impl UsageBudget {
    pub fn new(token_limit: Option<u64>, cost: Option<CostBudget>) -> Self {
        Self {
            token_limit,
            cost,
            spent_input: 0,
            spent_output: 0,
        }
    }

    /// Is the budget active (at least one cap configured)?
    pub fn is_active(&self) -> bool {
        self.token_limit.is_some() || self.cost.is_some()
    }

    /// Accounts for a turn (real OR estimated input + output).
    pub fn record(&mut self, input: u64, output: u64) {
        self.spent_input = self.spent_input.saturating_add(input);
        self.spent_output = self.spent_output.saturating_add(output);
    }

    pub fn record_usage(&mut self, usage: TokenUsage) {
        self.record(usage.input, usage.output);
    }

    pub fn spent_tokens(&self) -> u64 {
        self.spent_input.saturating_add(self.spent_output)
    }

    /// Breakdown of the cumulated usage, for observability (US-017). Accounted
    /// even without a configured cap: `is_active()` only gates the kill-switch.
    pub fn spent_input(&self) -> u64 {
        self.spent_input
    }

    pub fn spent_output(&self) -> u64 {
        self.spent_output
    }

    /// Cumulated cost in micro-USD (0 when no pricing is configured).
    pub fn spent_micro_usd(&self) -> u64 {
        match self.cost {
            Some(c) => micro_cost(self.spent_input, self.spent_output, &c),
            None => 0,
        }
    }

    /// Kill-switch: is the budget reached (>= 100%)? Tokens first, then
    /// cost.
    pub fn exceeded(&self) -> Option<ExhaustReason> {
        if let Some(limit) = self.token_limit {
            let spent = self.spent_tokens();
            if spent >= limit {
                return Some(ExhaustReason::TokenBudget { spent, limit });
            }
        }
        if let Some(c) = self.cost {
            let spent = micro_cost(self.spent_input, self.spent_output, &c);
            if spent >= c.limit_micro_usd {
                return Some(ExhaustReason::CostBudget {
                    spent_micro_usd: spent,
                    limit_micro_usd: c.limit_micro_usd,
                });
            }
        }
        None
    }

    /// Pre-turn estimate: projects the cost of the next turn (estimated input +
    /// output); if the projection crosses the cap, we stop BEFORE the turn.
    pub fn would_exceed(&self, next_input: u64, next_output: u64) -> Option<ExhaustReason> {
        if let Some(limit) = self.token_limit {
            let projected = self
                .spent_tokens()
                .saturating_add(next_input)
                .saturating_add(next_output);
            if projected >= limit {
                return Some(ExhaustReason::TokenBudget {
                    spent: projected,
                    limit,
                });
            }
        }
        if let Some(c) = self.cost {
            let projected =
                self.spent_micro_usd()
                    .saturating_add(micro_cost(next_input, next_output, &c));
            if projected >= c.limit_micro_usd {
                return Some(ExhaustReason::CostBudget {
                    spent_micro_usd: projected,
                    limit_micro_usd: c.limit_micro_usd,
                });
            }
        }
        None
    }
}

fn micro_cost(input: u64, output: u64, c: &CostBudget) -> u64 {
    // (tokens / 1000) * micro_per_ktok, in integer arithmetic (input*price/1000).
    let in_cost = input
        .saturating_mul(c.input_micro_per_ktok)
        .saturating_div(1000);
    let out_cost = output
        .saturating_mul(c.output_micro_per_ktok)
        .saturating_div(1000);
    in_cost.saturating_add(out_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_escalates_over_two_registers_before_it_aborts() {
        let mut g = LoopGuard::new(LOOP_GUARD_THRESHOLDS);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed); // 1
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed); // 2
        let gentle = LoopDecision::Signal(LoopRegister::Gentle);
        let detailed = LoopDecision::Signal(LoopRegister::Detailed);
        assert_eq!(g.observe("a".into()), gentle); // 3 = first tier
        assert_eq!(g.observe("a".into()), gentle); // 4 = still the first tier
        assert_eq!(g.observe("a".into()), detailed); // 5 = second tier
        assert_eq!(g.observe("a".into()), detailed); // 6
        assert_eq!(g.observe("a".into()), detailed); // 7
        assert_eq!(g.observe("a".into()), LoopDecision::Abort); // 8 = last tier
        assert_eq!(g.count(), 8, "the abort carries the real count, not 4");
    }

    /// Unhappy path of US-059: a run that stops short of the first tier leaves
    /// no residue, so the next batch starts a run of its own.
    #[test]
    fn a_different_batch_ends_the_run_without_crossing_any_tier() {
        let mut g = LoopGuard::new(LOOP_GUARD_THRESHOLDS);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed);
        assert_eq!(g.observe("b".into()), LoopDecision::Proceed);
        assert_eq!(g.count(), 1, "the count restarts at 1 on a new signature");
        assert_eq!(g.observe("b".into()), LoopDecision::Proceed);
        assert_eq!(
            g.observe("b".into()),
            LoopDecision::Signal(LoopRegister::Gentle)
        );
    }

    /// The `const _` block below the constant proves the ladder that ships; this
    /// proves the validator itself, without a `trybuild` dependency.
    #[test]
    fn the_validator_refuses_a_duplicate_a_decrease_and_a_hair_trigger() {
        assert!(loop_guard_thresholds_are_valid(&LOOP_GUARD_THRESHOLDS));
        assert!(
            !loop_guard_thresholds_are_valid(&[3, 3, 8]),
            "duplicate tier"
        );
        assert!(!loop_guard_thresholds_are_valid(&[8, 5, 3]), "decreasing");
        assert!(
            !loop_guard_thresholds_are_valid(&[1, 5, 8]),
            "a first tier under 2 cannot tell a loop from a retry"
        );
    }

    #[test]
    fn the_gentle_register_quotes_nothing_and_the_detailed_one_quotes_everything() {
        let args = serde_json::json!({"cmd": "ls -la"});
        let gentle = loop_guard_message(LoopRegister::Gentle, "bash", 3, &args);
        assert!(!gentle.contains("ls -la"), "gentle cost must stay constant");
        assert!(!gentle.contains("bash"));

        let detailed = loop_guard_message(LoopRegister::Detailed, "bash", 5, &args);
        assert!(detailed.contains("bash"));
        assert!(detailed.contains('5'));
        assert!(detailed.contains("ls -la"));
    }

    /// US-060 unhappy path + NFR-01. A multi-byte character straddling the
    /// ceiling must not produce a truncated code point, and the message stays
    /// bounded whatever the argument weighs.
    #[test]
    fn a_megabyte_argument_is_cut_on_a_character_boundary_under_the_ceiling() {
        // `{"a":"` is 6 bytes, so 493 filler bytes put a two-byte `e` acute at
        // bytes 499..=500: the ceiling falls inside it.
        let mut payload = "x".repeat(493);
        payload.push('\u{e9}');
        payload.push_str(&"y".repeat(1024 * 1024));
        let args = serde_json::json!({ "a": payload });
        let canonical = args.to_string();
        assert!(!canonical.is_char_boundary(LOOP_GUARD_ARGS_PREVIEW_BYTES));

        let message = loop_guard_message(LoopRegister::Detailed, "write", 5, &args);
        assert!(
            message.contains(&format!("{}... [", &canonical[..499])),
            "the cut must fall back to the previous character boundary"
        );
        assert!(
            !message.contains('\u{e9}'),
            "the straddling character is dropped whole, never split"
        );
        assert!(
            message.len() < LOOP_GUARD_ARGS_PREVIEW_BYTES + 256,
            "NFR-01: {} bytes",
            message.len()
        );
    }

    /// FR-06. The ceiling bounds the message, never the key: two bodies that
    /// differ only past the ceiling are two distinct signatures.
    #[test]
    fn the_detection_key_is_never_truncated_by_the_preview_ceiling() {
        let dispatch = Exempting("");
        let body = |tail: &str| {
            let mut value = "x".repeat(1024 * 1024);
            value.push_str(tail);
            ToolInvocation::json("x", "write", serde_json::json!({ "body": value }))
        };
        assert_ne!(
            guarded_batch_signature(&[body("left")], &dispatch),
            guarded_batch_signature(&[body("right")], &dispatch),
            "truncating the key would make every large write look like a loop"
        );
    }

    /// Dispatcher that exempts whatever it was told to exempt. WHICH calls are
    /// exempt is the tool's business (`agent-tools`); what the core owns is
    /// that the answer is honored.
    struct Exempting(&'static str);

    #[async_trait::async_trait]
    impl ToolDispatch for Exempting {
        fn loop_guard_exempt(&self, call: &ToolInvocation) -> bool {
            call.name == self.0
        }

        async fn dispatch(
            &self,
            _calls: Vec<ToolInvocation>,
            _events: crate::tools::ToolEventSink,
        ) -> Vec<crate::tools::ModelToolResult> {
            Vec::new()
        }
    }

    #[test]
    fn the_signature_honors_what_the_dispatcher_exempts() {
        let inv = |name: &str| ToolInvocation::json("x", name, serde_json::json!({"a": 1}));
        let dispatch = Exempting("control");

        assert_eq!(
            guarded_batch_signature(&[inv("control")], &dispatch),
            None,
            "a batch of exempt calls is not guardable at all"
        );
        assert!(guarded_batch_signature(&[inv("bash")], &dispatch).is_some());
        assert_eq!(
            guarded_batch_signature(&[inv("bash"), inv("control")], &dispatch),
            guarded_batch_signature(&[inv("bash")], &dispatch),
            "an exempt call must not change the signature of the guarded ones"
        );
    }

    /// US-063. The only thing that still breaks a run inside a turn is a human
    /// interjection, and this is the mechanism it uses.
    #[test]
    fn a_reset_breaks_a_repetition_run() {
        let mut guard = LoopGuard::new(LOOP_GUARD_THRESHOLDS);
        assert_eq!(guard.observe("same".into()), LoopDecision::Proceed);
        assert_eq!(guard.observe("same".into()), LoopDecision::Proceed);

        guard.reset();

        assert_eq!(guard.observe("same".into()), LoopDecision::Proceed);
        assert_eq!(guard.count(), 1);
    }

    #[test]
    fn batch_signature_is_order_independent_and_distinct() {
        let inv = |name: &str, input: serde_json::Value| ToolInvocation::json("x", name, input);
        let dispatch = Exempting("");
        let s1 = guarded_batch_signature(
            &[
                inv("read", serde_json::json!({"path": "a"})),
                inv("bash", serde_json::json!({"cmd": "ls"})),
            ],
            &dispatch,
        );
        let s2 = guarded_batch_signature(
            &[
                inv("bash", serde_json::json!({"cmd": "ls"})),
                inv("read", serde_json::json!({"path": "a"})),
            ],
            &dispatch,
        );
        assert_eq!(s1, s2, "call order should not matter");
        let s3 =
            guarded_batch_signature(&[inv("bash", serde_json::json!({"cmd": "pwd"}))], &dispatch);
        assert_ne!(s1, s3);
    }

    /// The signature of a single `write` whose arguments are parsed from raw
    /// text, the way a provider delta arrives. Going through `from_str` rather
    /// than `json!` keeps the two tests below on the path the arguments really
    /// take, key order included, instead of on a macro's expansion of it.
    fn signature_of(raw_arguments: &str) -> Option<String> {
        let input: serde_json::Value = serde_json::from_str(raw_arguments).unwrap();
        guarded_batch_signature(&[ToolInvocation::json("x", "write", input)], &Exempting(""))
    }

    /// US-067 / FR-12. Two argument objects differing only by the order of their
    /// keys, nested two levels down, are ONE signature, because a
    /// `serde_json::Map` is a `BTreeMap` as long as `preserve_order` stays off.
    /// This is the test that turns that assumption into a failure the day the
    /// feature is switched on transitively.
    #[test]
    fn the_signature_is_blind_to_the_order_of_the_json_keys() {
        assert_eq!(
            signature_of(r#"{"a":1,"b":{"c":2,"d":3}}"#),
            signature_of(r#"{"b":{"d":3,"c":2},"a":1}"#),
            "the key order must not split one repetition run into two"
        );
    }

    /// US-067 unhappy path: the blindness is to the ORDER of the keys and never
    /// to the values under them, otherwise the test above would pass by accident
    /// on a signature that has stopped discriminating anything.
    #[test]
    fn the_signature_tells_two_permuted_values_apart() {
        assert_ne!(
            signature_of(r#"{"a":1,"b":2}"#),
            signature_of(r#"{"a":2,"b":1}"#),
            "same keys, swapped values: two different calls"
        );
    }

    #[test]
    fn token_budget_kill_switch() {
        let mut b = UsageBudget::new(Some(1000), None);
        assert!(b.exceeded().is_none());
        b.record(600, 300); // 900 < 1000
        assert!(b.exceeded().is_none());
        b.record(100, 50); // 1050 >= 1000
        assert!(matches!(
            b.exceeded(),
            Some(ExhaustReason::TokenBudget {
                spent: 1050,
                limit: 1000
            })
        ));
    }

    #[test]
    fn pre_turn_estimate_stops_before_big_turn() {
        let b = UsageBudget::new(Some(1000), None);
        // nothing spent, but the next turn is projected at 1200 > 1000.
        assert!(matches!(
            b.would_exceed(900, 300),
            Some(ExhaustReason::TokenBudget { .. })
        ));
        assert!(b.would_exceed(500, 100).is_none());
    }

    #[test]
    fn cost_budget_kill_switch() {
        // 50 micro$/ktok input, 100 micro$/ktok output, cap 100 micro$.
        let cost = CostBudget {
            limit_micro_usd: 100,
            input_micro_per_ktok: 50,
            output_micro_per_ktok: 100,
        };
        let mut b = UsageBudget::new(None, Some(cost));
        b.record(1000, 500); // 1000*50/1000 + 500*100/1000 = 50 + 50 = 100 >= 100
        assert!(matches!(
            b.exceeded(),
            Some(ExhaustReason::CostBudget {
                spent_micro_usd: 100,
                limit_micro_usd: 100
            })
        ));
    }

    #[test]
    fn inactive_budget_never_triggers() {
        let mut b = UsageBudget::default();
        assert!(!b.is_active());
        b.record(1_000_000, 1_000_000);
        assert!(b.exceeded().is_none());
        assert!(b.would_exceed(1_000_000, 1_000_000).is_none());
    }
}
