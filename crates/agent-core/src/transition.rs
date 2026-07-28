//! State machine with typed transitions (ARCHITECTURE 3.1/3.5). The `enum
//! Transition` is exhaustive: the driver `match` (agent.rs) forces every case
//! to be handled -> control flow checked at compile time.
//!
//! Two PURE functions (no I/O), unit-testable, decide:
//! - `pre_stream_transition`: recover (withholding) / compaction / exhaustion,
//!   BEFORE the API call;
//! - `post_stream_transition`: EndTurn / RunTools / Fail, from the accumulator.

use std::collections::HashMap;

use crate::compaction::CompactKind;
use crate::error::{AgentError, ProviderFailure};
use crate::message::{ContentBlock, Message, Role, ToolCallId};
use crate::provider::{StopReason, StreamEvent};
use crate::tools::ToolInvocation;
use agent_tokenizer::TokenCounter;
use serde::{Deserialize, Serialize};

/// CONTEXT error held back by withholding (PTL / max-tokens). Distinct from
/// transient errors (backoff): invariant 8.
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
    /// Token budget kill-switch reached (US-014): `spent >= limit`.
    TokenBudget {
        spent: u64,
        limit: u64,
    },
    /// Cost budget kill-switch reached (US-014), in micro-USD (1e-6 $).
    CostBudget {
        spent_micro_usd: u64,
        limit_micro_usd: u64,
    },
    /// Tool loop persisting past the signal (US-014): deterministic stop
    /// after `count` identical repeats.
    ToolLoop {
        count: u32,
    },
    /// The model reached its output budget. This is not a clean end:
    /// the answer may be truncated, or even empty if reasoning consumed the
    /// budget.
    MaxOutputTokens {
        visible_output: bool,
    },
}

/// Exhaustive transition. Each variant is a decision event.
#[derive(Debug)]
pub enum Transition {
    /// The model finished without tool_use -> hand back control.
    EndTurn,
    /// Assistant output is committed, then the model is sampled again without a
    /// fabricated user message.
    Continue,
    /// The model asks for tools -> run them then loop back.
    RunTools(Vec<ToolInvocation>),
    /// Compaction (proactive auto, or reactive) before the next call.
    Compact(CompactKind),
    /// Context error held back (withholding), to recover before propagating.
    Recover(PendingError),
    /// US-001: cooperative cancellation signalled, stop at the current boundary,
    /// after transcript reconciliation (US-002).
    Interrupted,
    /// Turn cap / budget exhausted.
    Exhausted(ExhaustReason),
    /// Fatal, unrecoverable error.
    Fail(AgentError),
}

/// Decision BEFORE the API call. `None` means proceed to the stream. Priority:
/// withholding (recover) > exhaustion > proactive compaction.
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

/// Decision AFTER stream accumulation: EndTurn / RunTools / Fail.
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
        // Output truncated IN THE MIDDLE of a tool_call -> the tool intent is
        // incomplete and would be silently lost. max-tokens feeds withholding
        // (invariant 8 / ARCHITECTURE 3.4): reactive compaction then re-stream
        // to regenerate a complete call, instead of an EndTurn that drops it.
        Some(StopReason::MaxTokens) if !calls.is_empty() || acc.has_open_tool_calls() => {
            Transition::Recover(PendingError {
                kind: ContextErrorKind::MaxTokensInput,
            })
        }
        Some(StopReason::MaxTokens) => Transition::Exhausted(ExhaustReason::MaxOutputTokens {
            visible_output: acc.has_visible_output(),
        }),
        Some(StopReason::ContentFilter) => Transition::Fail(AgentError::Provider(
            ProviderFailure::contract("model output blocked by content filter"),
        )),
        Some(StopReason::IncompleteUnknown) => Transition::Fail(AgentError::Provider(
            ProviderFailure::contract("model response incomplete for an unknown reason"),
        )),
        Some(StopReason::Refusal) => Transition::Fail(AgentError::Provider(
            ProviderFailure::contract("model refusal"),
        )),
        Some(StopReason::Continue) => Transition::Continue,
        Some(StopReason::EndTurn | StopReason::StopSequence) => Transition::EndTurn,
    }
}

// ───────────────────────────── Accumulator ─────────────────────────────

struct PartialCall {
    name: String,
    args: String,
}

struct CompletedCall {
    name: String,
    input: serde_json::Value,
}

/// Accumulates `StreamEvent`s (except `Usage`, handled by the budget) into a
/// decision state.
#[derive(Default)]
pub struct Accumulator {
    text: String,
    reasoning: String,
    pub stop: Option<StopReason>,
    open: HashMap<ToolCallId, PartialCall>,
    completed: HashMap<ToolCallId, CompletedCall>,
    order: Vec<ToolCallId>,
    /// Captured encrypted reasoning items (US-031, isolated replay): `(id, content)`,
    /// in arrival order (before their function_calls). Empty when replay is OFF.
    reasonings: Vec<(String, String)>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds in an event (`Usage` is handled by the budget, not here).
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
            // Telemetry, not turn content: consumed by the loop, nothing to
            // accumulate for the assistant message.
            StreamEvent::Usage { .. } | StreamEvent::Quota { .. } => {}
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

    /// Complete tool calls, validated at `ToolCallEnd`.
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

    /// Builds the assistant message to persist (text + thinking + tool_use).
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
        // US-031: encrypted reasoning items BEFORE the function_calls (rs/fc pair).
        // Empty when replay is OFF -> message identical to the flat path.
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

    /// Conservative estimate of the generated output when the provider did not
    /// send an `Usage`. Includes visible text, reasoning, tool calls and any
    /// incomplete fragments before an error.
    pub fn estimate_output(&self, counter: &dyn TokenCounter) -> u32 {
        let mut total = counter
            .count_text(&self.text)
            .saturating_add(counter.count_text(&self.reasoning));
        for (id, encrypted_content) in &self.reasonings {
            total = total
                .saturating_add(counter.count_text(id))
                .saturating_add(counter.count_text(encrypted_content));
        }
        for call in self.completed.values() {
            total = total
                .saturating_add(counter.count_text(&call.name))
                .saturating_add(counter.count_text(&call.input.to_string()));
        }
        for call in self.open.values() {
            total = total
                .saturating_add(counter.count_text(&call.name))
                .saturating_add(counter.count_text(&call.args));
        }
        u32::try_from(total).unwrap_or(u32::MAX)
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
    use agent_tokenizer::HeuristicCounter;

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
            other => unreachable!("expected RunTools, got {other:?}"),
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
        // truncated mid tool_call -> Recover (withholding), not a silent EndTurn
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
        // text truncated with no tool_call in flight: non-success, because reasoning
        // may have consumed the budget before a complete answer.
        let a = acc_with(vec![
            StreamEvent::TextDelta {
                text: "truncated answer".into(),
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

    // US-031: the Accumulator captures reasoning items and places them BEFORE the
    // tool_use in the assistant message.
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
        assert!(rs < tu, "reasoning before tool_use");
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
    fn fallback_output_estimate_counts_reasoning_and_tool_calls() {
        let a = acc_with(vec![
            StreamEvent::ReasoningDelta {
                text: "hidden reasoning".into(),
            },
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
                args_json: "{\"cmd\":\"ls\"}".into(),
            },
            StreamEvent::ToolCallEnd { id: "c1".into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]);
        assert!(
            a.estimate_output(&HeuristicCounter) > 0,
            "output without visible text should still count"
        );
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
