//! Garde-fous déterministes (US-014, EP-003) — ils OVERRIDENT la logique
//! faillible du modèle, depuis l'extérieur de la boucle :
//!
//! - `LoopGuard` : détecte le même batch d'outils (mêmes noms + mêmes args)
//!   répété N fois de suite. Au Nème → **signal explicite** à l'agent (le batch
//!   n'est PAS exécuté) ; s'il persiste → **abandon** déterministe.
//! - `UsageBudget` : budget cumulé tokens/coût avec **kill-switch** à 100 % et
//!   estimation pré-tour pour stopper *avant* un tour trop coûteux.
//!
//! Ces garde-fous vivent dans `agent-core` (et non `agent-tools`) car le graphe
//! de dépendances interdit `core → tools`, et l'arrêt de boucle (`Exhausted`)
//! est une décision de terminaison qui appartient au cœur. Pures, sans I/O →
//! testables unitairement.

use crate::provider::TokenUsage;
use crate::tools::ToolInvocation;
use crate::transition::ExhaustReason;

/// Décision du garde-fou de boucle pour un batch d'outils.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    /// Pas de boucle : exécuter normalement.
    Proceed,
    /// Nème répétition identique : ne pas exécuter, signaler à l'agent.
    Signal,
    /// Répétition au-delà du signal : arrêt déterministe de la boucle.
    Abort,
}

/// Détecteur de boucle d'outils (ARCHITECTURE §3 garde-fous / FR-05). Compare la
/// signature du batch courant à la précédente ; compte les répétitions
/// consécutives.
#[derive(Debug)]
pub struct LoopGuard {
    threshold: u32,
    last_sig: Option<String>,
    count: u32,
}

impl LoopGuard {
    /// `threshold` = nombre de répétitions identiques avant signal (défaut 3).
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

    /// Intègre la signature du batch courant et décide.
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

/// Signature déterministe d'un batch d'appels : `nom\0json` par appel, joints.
/// Le `Display` de `serde_json::Value` produit un JSON compact aux clés triées
/// (`serde_json::Map` sans `preserve_order`) → signature stable d'un tour à
/// l'autre. L'ordre des appels dans le batch ne change pas la signature.
pub fn batch_signature(calls: &[ToolInvocation]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| format!("{}\u{0}{}", c.name, c.input))
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

/// Tarif d'un modèle, en micro-USD (1e-6 $) par millier de tokens. `u64` pour
/// rester `Copy`/`Eq` (pas de `f64` dans la config / l'`ExhaustReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostBudget {
    pub limit_micro_usd: u64,
    pub input_micro_per_ktok: u64,
    pub output_micro_per_ktok: u64,
}

/// Budget cumulé de la session (tokens et/ou coût). Désactivé par défaut
/// (`token_limit`/`cost` à `None`) → aucun impact sur les sessions sans budget.
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

    /// Budget actif (au moins un plafond configuré) ?
    pub fn is_active(&self) -> bool {
        self.token_limit.is_some() || self.cost.is_some()
    }

    /// Comptabilise un tour (input + output réels OU estimés).
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

    /// Coût cumulé en micro-USD (0 si aucun tarif configuré).
    pub fn spent_micro_usd(&self) -> u64 {
        match self.cost {
            Some(c) => micro_cost(self.spent_input, self.spent_output, &c),
            None => 0,
        }
    }

    /// Kill-switch : le budget est-il atteint (≥ 100 %) ? Priorité tokens puis
    /// coût.
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

    /// Estimation pré-tour : projette le coût du prochain tour (input + output
    /// estimés) ; si la projection franchit le plafond, on stoppe AVANT le tour.
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
    // (tokens / 1000) * micro_par_ktok, en arithmétique entière (input*prix/1000).
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
        assert_eq!(g.observe("a".into()), LoopDecision::Signal); // 3 = seuil
        assert_eq!(g.observe("a".into()), LoopDecision::Abort); // 4 > seuil
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
        assert_eq!(s1, s2, "l'ordre des appels ne doit pas compter");
        let s3 = batch_signature(&[inv("bash", serde_json::json!({"cmd": "pwd"}))]);
        assert_ne!(s1, s3);
    }

    #[test]
    fn token_budget_kill_switch() {
        let mut b = UsageBudget::new(Some(1000), None);
        assert!(b.exceeded().is_none());
        b.record(600, 300); // 900 < 1000
        assert!(b.exceeded().is_none());
        b.record(100, 50); // 1050 ≥ 1000
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
        // rien dépensé, mais le prochain tour projeté à 1200 > 1000.
        assert!(matches!(
            b.would_exceed(900, 300),
            Some(ExhaustReason::TokenBudget { .. })
        ));
        assert!(b.would_exceed(500, 100).is_none());
    }

    #[test]
    fn cost_budget_kill_switch() {
        // 50 micro$/ktok input, 100 micro$/ktok output, plafond 100 micro$.
        let cost = CostBudget {
            limit_micro_usd: 100,
            input_micro_per_ktok: 50,
            output_micro_per_ktok: 100,
        };
        let mut b = UsageBudget::new(None, Some(cost));
        b.record(1000, 500); // 1000*50/1000 + 500*100/1000 = 50 + 50 = 100 ≥ 100
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
