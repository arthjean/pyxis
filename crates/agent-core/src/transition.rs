//! State machine à transitions typées (ARCHITECTURE §3.1/§3.5). L'`enum
//! Transition` est exhaustif : le `match` du driver (agent.rs) force le
//! traitement de tous les cas → contrôle de flux vérifié à la compilation.
//!
//! Deux fonctions PURES (sans I/O), testables unitairement, décident :
//! - `pre_stream_transition` : recover (withholding) / compaction / épuisement,
//!   AVANT l'appel API ;
//! - `post_stream_transition` : EndTurn / RunTools / Fail, d'après l'accumulateur.

use std::collections::HashMap;

use crate::compaction::CompactKind;
use crate::error::{AgentError, ProviderFailure};
use crate::message::{ContentBlock, Message, Role, ToolCallId};
use crate::provider::{StopReason, StreamEvent};
use crate::tools::ToolInvocation;
use serde::{Deserialize, Serialize};

/// Erreur de CONTEXTE retenue par le withholding (PTL / max-tokens). Distincte
/// des erreurs transitoires (backoff) — invariant 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextErrorKind {
    PromptTooLong,
    MaxTokensInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingError {
    pub kind: ContextErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustReason {
    MaxTurns(u32),
    /// Kill-switch budget tokens atteint (US-014) : `spent ≥ limit`.
    TokenBudget {
        spent: u64,
        limit: u64,
    },
    /// Kill-switch budget coût atteint (US-014), en micro-USD (1e-6 $).
    CostBudget {
        spent_micro_usd: u64,
        limit_micro_usd: u64,
    },
    /// Boucle d'outils persistante au-delà du signal (US-014) : arrêt
    /// déterministe après `count` répétitions identiques.
    ToolLoop {
        count: u32,
    },
    /// Le modèle a atteint son budget de sortie. Ce n'est pas une fin propre :
    /// la réponse peut être tronquée, voire vide si le reasoning a consommé le
    /// budget.
    MaxOutputTokens {
        visible_output: bool,
    },
}

/// Transition exhaustive. Chaque variante est un événement décisionnel.
#[derive(Debug)]
pub enum Transition {
    /// Le modèle a fini sans tool_use → rendre la main.
    EndTurn,
    /// Le modèle demande des outils → exécuter puis reboucler.
    RunTools(Vec<ToolInvocation>),
    /// Compaction (proactive auto, ou réactive) avant le prochain appel.
    Compact(CompactKind),
    /// Erreur de contexte retenue (withholding) à récupérer avant de propager.
    Recover(PendingError),
    /// Plafond de tours / budget épuisé.
    Exhausted(ExhaustReason),
    /// Erreur fatale non récupérable.
    Fail(AgentError),
}

/// Décision AVANT l'appel API. `None` ⇒ procéder au stream. Priorité :
/// withholding (recover) > épuisement > compaction proactive.
pub fn pre_stream_transition(
    pending: Option<PendingError>,
    model_turns: u32,
    max_turns: u32,
    should_autocompact: bool,
) -> Option<Transition> {
    if let Some(p) = pending {
        return Some(Transition::Recover(p));
    }
    if model_turns >= max_turns {
        return Some(Transition::Exhausted(ExhaustReason::MaxTurns(max_turns)));
    }
    if should_autocompact {
        return Some(Transition::Compact(CompactKind::Auto));
    }
    None
}

/// Décision APRÈS l'accumulation du stream : EndTurn / RunTools / Fail.
pub fn post_stream_transition(acc: &Accumulator) -> Transition {
    let calls = acc.tool_calls();
    match acc.stop {
        None => Transition::Fail(AgentError::Provider(ProviderFailure::contract(
            "provider stream ended without terminal event",
        ))),
        Some(StopReason::ToolUse) if !calls.is_empty() && !acc.has_open_tool_calls() => {
            Transition::RunTools(calls)
        }
        Some(StopReason::ToolUse) if acc.has_open_tool_calls() => {
            Transition::Fail(AgentError::Provider(ProviderFailure::contract(
                "provider ended with an open tool call",
            )))
        }
        Some(StopReason::ToolUse) => Transition::Fail(AgentError::Provider(
            ProviderFailure::contract("provider signaled tool_use without tool calls"),
        )),
        // Output tronqué EN PLEIN tool_call → l'intention d'outil est incomplète
        // et serait silencieusement perdue. max-tokens alimente le withholding
        // (invariant 8 / ARCHITECTURE §3.4) : compaction réactive puis re-stream
        // pour régénérer un appel complet, au lieu d'un EndTurn qui jette le call.
        Some(StopReason::MaxTokens) if !calls.is_empty() || acc.has_open_tool_calls() => {
            Transition::Recover(PendingError {
                kind: ContextErrorKind::MaxTokensInput,
            })
        }
        Some(StopReason::MaxTokens) => Transition::Exhausted(ExhaustReason::MaxOutputTokens {
            visible_output: acc.has_visible_output(),
        }),
        Some(StopReason::Refusal) => Transition::Fail(AgentError::Provider(
            ProviderFailure::contract("refus du modèle"),
        )),
        Some(StopReason::EndTurn | StopReason::StopSequence) => Transition::EndTurn,
    }
}

// ───────────────────────────── Accumulateur ─────────────────────────────

struct PartialCall {
    name: String,
    args: String,
}

struct CompletedCall {
    name: String,
    input: serde_json::Value,
}

/// Accumule les `StreamEvent` (hors `Usage`, géré par le budget) en un état
/// décisionnel.
#[derive(Default)]
pub struct Accumulator {
    text: String,
    reasoning: String,
    pub stop: Option<StopReason>,
    open: HashMap<ToolCallId, PartialCall>,
    completed: HashMap<ToolCallId, CompletedCall>,
    order: Vec<ToolCallId>,
    /// Reasoning items chiffrés capturés (US-031, replay isolé) : `(id, contenu)`,
    /// dans l'ordre d'arrivée (avant leurs function_calls). Vides si replay OFF.
    reasonings: Vec<(String, String)>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intègre un événement (le `Usage` est traité par le budget, pas ici).
    pub fn push(&mut self, ev: StreamEvent) -> Result<(), AgentError> {
        match ev {
            StreamEvent::TextDelta { text } => self.text.push_str(&text),
            StreamEvent::ReasoningDelta { text } => self.reasoning.push_str(&text),
            StreamEvent::ToolCallStart { id, name } => {
                if id.trim().is_empty() {
                    return Err(contract_error("tool call id is empty"));
                }
                if name.trim().is_empty() {
                    return Err(contract_error(format!("tool call {id} name is empty")));
                }
                if self.open.contains_key(&id) || self.completed.contains_key(&id) {
                    return Err(contract_error(format!("duplicate tool call id: {id}")));
                }
                self.open.insert(
                    id.clone(),
                    PartialCall {
                        name,
                        args: String::new(),
                    },
                );
                self.order.push(id);
            }
            StreamEvent::ToolCallDelta { id, args_json } => {
                if let Some(p) = self.open.get_mut(&id) {
                    p.args.push_str(&args_json);
                } else {
                    return Err(contract_error(format!(
                        "tool call delta without start: {id}"
                    )));
                }
            }
            StreamEvent::EncryptedReasoning {
                id,
                encrypted_content,
            } => self.reasonings.push((id, encrypted_content)),
            StreamEvent::ToolCallEnd { id } => {
                let Some(partial) = self.open.remove(&id) else {
                    return Err(contract_error(format!("tool call end without start: {id}")));
                };
                let input = parse_tool_args(&id, &partial.args)?;
                self.completed.insert(
                    id,
                    CompletedCall {
                        name: partial.name,
                        input,
                    },
                );
            }
            StreamEvent::Usage { .. } => {}
            StreamEvent::Done { stop } => self.stop = Some(stop),
        }
        Ok(())
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn has_open_tool_calls(&self) -> bool {
        !self.open.is_empty()
    }

    pub fn has_visible_output(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty()
    }

    /// Appels d'outils complets, validés à `ToolCallEnd`.
    pub fn tool_calls(&self) -> Vec<ToolInvocation> {
        self.order
            .iter()
            .filter_map(|id| {
                self.completed.get(id).map(|p| ToolInvocation {
                    id: id.clone(),
                    name: p.name.clone(),
                    input: p.input.clone(),
                })
            })
            .collect()
    }

    /// Construit le message assistant à persister (text + thinking + tool_use).
    pub fn to_assistant_message(&self) -> Message {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::Thinking {
                text: self.reasoning.clone(),
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }
        // US-031 : reasoning items chiffrés AVANT les function_calls (paire rs/fc).
        // Vide si replay OFF → message identique au chemin plat.
        for (id, encrypted_content) in &self.reasonings {
            content.push(ContentBlock::EncryptedReasoning {
                id: id.clone(),
                encrypted_content: encrypted_content.clone(),
            });
        }
        for id in &self.order {
            if let Some(p) = self.completed.get(id) {
                content.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: p.name.clone(),
                    input: p.input.clone(),
                });
            }
        }
        Message {
            role: Role::Assistant,
            content,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.is_empty()
            && self.reasonings.is_empty()
            && self.completed.is_empty()
    }
}

fn parse_tool_args(id: &str, args: &str) -> Result<serde_json::Value, AgentError> {
    if args.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(args)
        .map_err(|e| contract_error(format!("invalid JSON arguments for tool call {id}: {e}")))
}

fn contract_error(message: impl Into<String>) -> AgentError {
    AgentError::Provider(ProviderFailure::contract(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc_with(events: Vec<StreamEvent>) -> Accumulator {
        let mut a = Accumulator::new();
        for e in events {
            a.push(e).unwrap();
        }
        a
    }

    #[test]
    fn pre_stream_priority_recover_then_exhaust_then_compact() {
        let p = PendingError {
            kind: ContextErrorKind::PromptTooLong,
        };
        assert!(matches!(
            pre_stream_transition(Some(p), 0, 10, true),
            Some(Transition::Recover(_))
        ));
        assert!(matches!(
            pre_stream_transition(None, 10, 10, true),
            Some(Transition::Exhausted(ExhaustReason::MaxTurns(10)))
        ));
        assert!(matches!(
            pre_stream_transition(None, 0, 10, true),
            Some(Transition::Compact(CompactKind::Auto))
        ));
        assert!(pre_stream_transition(None, 0, 10, false).is_none());
    }

    #[test]
    fn post_stream_endturn_runtools_fail() {
        let end = acc_with(vec![StreamEvent::Done {
            stop: StopReason::EndTurn,
        }]);
        assert!(matches!(post_stream_transition(&end), Transition::EndTurn));

        let tools = acc_with(vec![
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: "c1".into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]);
        match post_stream_transition(&tools) {
            Transition::RunTools(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "bash");
                assert_eq!(calls[0].input["cmd"], "ls");
            }
            other => unreachable!("attendu RunTools, eu {other:?}"),
        }

        let refusal = acc_with(vec![StreamEvent::Done {
            stop: StopReason::Refusal,
        }]);
        assert!(matches!(
            post_stream_transition(&refusal),
            Transition::Fail(_)
        ));
    }

    #[test]
    fn tooluse_stop_without_calls_is_failclosed_endturn() {
        let a = acc_with(vec![StreamEvent::Done {
            stop: StopReason::ToolUse,
        }]);
        assert!(matches!(post_stream_transition(&a), Transition::Fail(_)));
    }

    #[test]
    fn maxtokens_mid_toolcall_recovers_not_drops() {
        // tronqué en plein tool_call → Recover (withholding), pas EndTurn silencieux
        let a = acc_with(vec![
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{\"cm".into(),
            },
            StreamEvent::Done {
                stop: StopReason::MaxTokens,
            },
        ]);
        assert!(matches!(
            post_stream_transition(&a),
            Transition::Recover(PendingError {
                kind: ContextErrorKind::MaxTokensInput
            })
        ));
    }

    #[test]
    fn maxtokens_plain_text_exhausts() {
        // texte tronqué sans tool_call en cours : non-success, car le reasoning
        // peut avoir consommé le budget avant une réponse complète.
        let a = acc_with(vec![
            StreamEvent::TextDelta {
                text: "réponse coupée".into(),
            },
            StreamEvent::Done {
                stop: StopReason::MaxTokens,
            },
        ]);
        assert!(matches!(
            post_stream_transition(&a),
            Transition::Exhausted(ExhaustReason::MaxOutputTokens {
                visible_output: true
            })
        ));
    }

    #[test]
    fn missing_terminal_event_fails_closed() {
        let a = acc_with(vec![StreamEvent::TextDelta {
            text: "partiel".into(),
        }]);
        assert!(matches!(post_stream_transition(&a), Transition::Fail(_)));
    }

    // US-031 : l'Accumulator capture les reasoning items et les place AVANT les
    // tool_use dans le message assistant.
    #[test]
    fn accumulator_orders_reasoning_before_tooluse() {
        let a = acc_with(vec![
            StreamEvent::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into(),
            },
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{}".into(),
            },
            StreamEvent::ToolCallEnd { id: "c1".into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]);
        let m = a.to_assistant_message();
        let rs = m
            .content
            .iter()
            .position(|b| matches!(b, ContentBlock::EncryptedReasoning { .. }))
            .unwrap();
        let tu = m
            .content
            .iter()
            .position(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .unwrap();
        assert!(rs < tu, "reasoning avant tool_use");
    }

    #[test]
    fn assistant_message_carries_text_and_tooluse() {
        let a = acc_with(vec![
            StreamEvent::TextDelta { text: "ok".into() },
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{}".into(),
            },
            StreamEvent::ToolCallEnd { id: "c1".into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]);
        let m = a.to_assistant_message();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 2);
    }

    #[test]
    fn accumulator_rejects_delta_without_start() {
        let mut a = Accumulator::new();
        let err = a
            .push(StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_json: "{}".into(),
            })
            .unwrap_err();
        assert!(matches!(err, AgentError::Provider(_)));
    }

    #[test]
    fn accumulator_rejects_invalid_json_at_tool_end() {
        let mut a = Accumulator::new();
        a.push(StreamEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        })
        .unwrap();
        a.push(StreamEvent::ToolCallDelta {
            id: "c1".into(),
            args_json: "{\"cmd\"".into(),
        })
        .unwrap();
        let err = a
            .push(StreamEvent::ToolCallEnd { id: "c1".into() })
            .unwrap_err();
        assert!(matches!(err, AgentError::Provider(_)));
    }
}
