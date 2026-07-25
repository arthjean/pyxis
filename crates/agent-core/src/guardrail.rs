//! Deterministic guardrails (US-014, EP-003). They OVERRIDE the model's
//! fallible logic, from outside the loop:
//!
//! - `LoopGuard`: detects the same tool batch (same names + same args)
//!   repeated N times in a row. On the Nth -> **explicit signal** to the agent (the
//!   batch is NOT executed); if it persists -> deterministic **abort**.
//! - `UsageBudget`: cumulated token/cost budget with a **kill-switch** at 100% and
//!   a pre-turn estimate to stop *before* a turn that costs too much.
//!
//! These guardrails live in `agent-core` (and not `agent-tools`) because the
//! dependency graph forbids `core -> tools`, and stopping the loop (`Exhausted`)
//! is a termination decision that belongs to the core. Pure, no I/O ->
//! unit-testable.

use crate::provider::TokenUsage;
use crate::tools::ToolInvocation;
use crate::transition::ExhaustReason;

/// Loop guardrail decision for a tool batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    /// No loop: run normally.
    Proceed,
    /// Nth identical repeat: do not execute, signal the agent.
    Signal,
    /// Repeat past the signal: deterministic stop of the loop.
    Abort,
}

/// Tool loop detector (ARCHITECTURE section 3 guardrails / FR-05). Compares the
/// signature of the current batch to the previous one; counts consecutive
/// repeats.
#[derive(Debug)]
pub struct LoopGuard {
    threshold: u32,
    last_sig: Option<String>,
    count: u32,
}

impl LoopGuard {
    /// `threshold` = number of identical repeats before the signal (default 3).
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            last_sig: None,
            count: 0,
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Folds in the signature of the current batch and decides.
    pub fn observe(&mut self, signature: String) -> LoopDecision {
        if self.last_sig.as_deref() == Some(signature.as_str()) {
            self.count = self.count.saturating_add(1);
        } else {
            self.last_sig = Some(signature);
            self.count = 1;
        }
        if self.count < self.threshold {
            LoopDecision::Proceed
        } else if self.count == self.threshold {
            LoopDecision::Signal
        } else {
            LoopDecision::Abort
        }
    }
}

/// Deterministic signature of a call batch: `name\0json` per call, joined.
/// The `Display` of `serde_json::Value` produces compact JSON with sorted keys
/// (`serde_json::Map` without `preserve_order`) -> signature stable from one turn
/// to the next. The order of calls in the batch does not change the signature.
pub fn batch_signature(calls: &[ToolInvocation]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| format!("{}\u{0}{}", c.name, c.input))
        .collect();
    parts.sort();
    parts.join("\u{1}")
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
        self.record(usage.input as u64, usage.output as u64);
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
    fn loop_guard_signals_then_aborts() {
        let mut g = LoopGuard::new(3);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed); // 1
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed); // 2
        assert_eq!(g.observe("a".into()), LoopDecision::Signal); // 3 = threshold
        assert_eq!(g.observe("a".into()), LoopDecision::Abort); // 4 > threshold
    }

    #[test]
    fn loop_guard_resets_on_different_batch() {
        let mut g = LoopGuard::new(3);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed);
        assert_eq!(g.observe("a".into()), LoopDecision::Proceed);
        assert_eq!(g.observe("b".into()), LoopDecision::Proceed); // reset
        assert_eq!(g.observe("b".into()), LoopDecision::Proceed);
        assert_eq!(g.observe("b".into()), LoopDecision::Signal);
    }

    #[test]
    fn batch_signature_is_order_independent_and_distinct() {
        let inv = |name: &str, input: serde_json::Value| ToolInvocation {
            id: "x".into(),
            name: name.into(),
            input,
        };
        let s1 = batch_signature(&[
            inv("read", serde_json::json!({"path": "a"})),
            inv("bash", serde_json::json!({"cmd": "ls"})),
        ]);
        let s2 = batch_signature(&[
            inv("bash", serde_json::json!({"cmd": "ls"})),
            inv("read", serde_json::json!({"path": "a"})),
        ]);
        assert_eq!(s1, s2, "call order should not matter");
        let s3 = batch_signature(&[inv("bash", serde_json::json!({"cmd": "pwd"}))]);
        assert_ne!(s1, s3);
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
