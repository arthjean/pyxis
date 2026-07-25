//! `ContextBudget`: computed ONCE per model (invariant 5), single source of
//! truth for micro/auto-compaction. Reads the stream `usage` when present,
//! otherwise falls back on the local tokenizer (provider without `usage` in the
//! stream, ARCHITECTURE 3.3 / PROVIDERS 4.3; also serves the US-014 pre-turn estimate).

use agent_tokenizer::TokenCounter;

use crate::message::{ContentBlock, Message};
use crate::provider::{TokenUsage, ToolSpec};

#[derive(Debug, Clone)]
pub struct ContextBudget {
    max_context: u32,
    output_reserve: u32,
    micro_threshold: u32,
    auto_threshold: u32,
    current_input: u32,
    usage_seen: bool,
    /// Post-compaction baseline (US-030): incompressible input right AFTER a
    /// compaction (summary + system + tools + context). Thresholds measure
    /// `current - prefill` (the GROWTH since the last compaction), never the
    /// absolute value, otherwise the fixed overhead re-triggers an immediate
    /// compaction (double compaction, risk #6). Default 0 -> original absolute behavior.
    prefill_input: u32,
    /// True between a successful compaction and the FIRST real `usage` after it: that
    /// first usage becomes the `prefill` (anchored on backend usage, not the estimate).
    awaiting_baseline: bool,
}

impl ContextBudget {
    /// Fallible variant for the runtime wiring: an unusable window is a
    /// config/provider error, not a budget with zeroed thresholds.
    pub fn try_for_model(max_context: u32, output_reserve: u32) -> Result<Self, String> {
        if max_context == 0 {
            return Err("max_context provider nul".to_string());
        }
        if output_reserve == 0 {
            return Err("max_output_tokens nul".to_string());
        }
        if output_reserve >= max_context {
            return Err(format!(
                "max_output_tokens ({output_reserve}) must be lower than context ({max_context})"
            ));
        }
        Ok(Self::for_model(max_context, output_reserve))
    }

    /// Builds the budget from the model window. Thresholds: micro at 70%,
    /// auto at 80% of the usable window (`max_context - output_reserve`).
    pub fn for_model(max_context: u32, output_reserve: u32) -> Self {
        let usable = max_context.saturating_sub(output_reserve);
        Self {
            max_context,
            output_reserve,
            micro_threshold: pct(usable, 70),
            auto_threshold: pct(usable, 80),
            current_input: 0,
            usage_seen: false,
            prefill_input: 0,
            awaiting_baseline: false,
        }
    }

    pub fn max_context(&self) -> u32 {
        self.max_context
    }
    pub fn output_reserve(&self) -> u32 {
        self.output_reserve
    }
    pub fn micro_threshold(&self) -> u32 {
        self.micro_threshold
    }
    pub fn auto_threshold(&self) -> u32 {
        self.auto_threshold
    }
    pub fn current_input(&self) -> u32 {
        self.current_input
    }
    pub fn usage_seen(&self) -> bool {
        self.usage_seen
    }

    /// New turn: we reset the `usage_seen` flag (the current count itself
    /// reflects the context state and is not reset).
    pub fn begin_turn(&mut self) {
        self.usage_seen = false;
    }

    /// Nominal path: consumes the `usage` emitted by the stream. US-030: the FIRST
    /// real usage after a compaction becomes the `prefill` baseline (incompressible
    /// overhead), so that we then measure growth and not the absolute value.
    pub fn observe_usage(&mut self, usage: TokenUsage) {
        self.current_input = usage.input;
        self.usage_seen = true;
        if self.awaiting_baseline {
            self.prefill_input = usage.input;
            self.awaiting_baseline = false;
        }
    }

    /// Fallback (provider without usage): feeds the threshold with a local
    /// estimate. Does NOT set `usage_seen` (it is an estimate, not a real signal),
    /// NOR the baseline (anchored only on REAL usage, US-030).
    pub fn observe_estimated(&mut self, estimated_input: u32) {
        self.current_input = estimated_input;
    }

    /// US-030: signals that a compaction just succeeded. The estimated compacted
    /// context becomes an immediate baseline to avoid a double compaction before
    /// the next stream; the next real `usage` will replace it.
    pub fn mark_compacted(&mut self, compacted_input: u32) {
        self.current_input = compacted_input;
        self.prefill_input = compacted_input;
        self.awaiting_baseline = true;
    }

    pub fn prefill_input(&self) -> u32 {
        self.prefill_input
    }

    pub fn should_microcompact(&self) -> bool {
        self.current_input.saturating_sub(self.prefill_input) >= self.micro_threshold
    }
    pub fn should_autocompact(&self) -> bool {
        self.current_input.saturating_sub(self.prefill_input) >= self.auto_threshold
    }

    /// US-030 (MidTurn): projects whether a GIVEN input would trigger autocompaction,
    /// WITHOUT mutating the budget based on real usage. Used to detect a long
    /// `tool_result` that just crossed the threshold, so we compact before the next
    /// model turn.
    pub fn would_autocompact(&self, projected_input: u32) -> bool {
        projected_input.saturating_sub(self.prefill_input) >= self.auto_threshold
    }
}

fn pct(v: u32, p: u32) -> u32 {
    ((u64::from(v) * u64::from(p)) / 100) as u32
}

/// Estimates the input tokens of a transcript through a `TokenCounter` (fallback
/// when `usage` is not provided). Approximates images to 0.
pub fn estimate_input(messages: &[Message], counter: &dyn TokenCounter) -> u32 {
    let mut total = 0usize;
    for m in messages {
        for b in &m.content {
            total += match b {
                ContentBlock::Text { text }
                | ContentBlock::Thinking { text }
                | ContentBlock::Summary { text, .. } => counter.count_text(text),
                ContentBlock::ToolUse { name, input, .. } => {
                    counter.count_text(name) + counter.count_text(&input.to_string())
                }
                ContentBlock::ToolResult { content, .. } => counter.count_text(content),
                ContentBlock::Image { .. } => 0,
                // US-031: encrypted reasoning is sent to the backend when replay
                // is active -> it counts in the budget (otherwise absent from messages).
                ContentBlock::EncryptedReasoning {
                    encrypted_content, ..
                } => counter.count_text(encrypted_content),
            };
        }
    }
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Estimates the static overhead sent with every request: system prompt,
/// ephemeral context and tool schemas. The backend counts these tokens in
/// `usage.input`; local projections must therefore include them too.
pub fn estimate_static_input(
    system: &Option<String>,
    context_messages: &[Message],
    tools: &[ToolSpec],
    counter: &dyn TokenCounter,
) -> u32 {
    let mut total = system
        .as_deref()
        .map(|s| counter.count_text(s))
        .unwrap_or_default();
    total = total.saturating_add(estimate_input(context_messages, counter) as usize);
    for tool in tools {
        total = total
            .saturating_add(counter.count_text(&tool.name))
            .saturating_add(counter.count_text(&tool.description))
            .saturating_add(counter.count_text(&tool.input_schema.to_string()));
    }
    u32::try_from(total).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tokenizer::HeuristicCounter;

    #[test]
    fn budget_thresholds_from_single_source() {
        // window 1000, reserve 200 -> usable 800; micro 560, auto 640.
        let b = ContextBudget::for_model(1000, 200);
        assert_eq!(b.output_reserve(), 200);
        assert_eq!(b.micro_threshold(), 560);
        assert_eq!(b.auto_threshold(), 640);
        assert!(!b.should_microcompact());
        assert!(!b.should_autocompact());
    }

    #[test]
    fn invalid_context_geometry_is_rejected() {
        assert!(ContextBudget::try_for_model(0, 200).is_err());
        assert!(ContextBudget::try_for_model(1000, 0).is_err());
        assert!(ContextBudget::try_for_model(1000, 1000).is_err());
        assert!(ContextBudget::try_for_model(1000, 1001).is_err());
        assert!(ContextBudget::try_for_model(1000, 200).is_ok());
    }

    #[test]
    fn usage_seen_vs_estimated() {
        let mut b = ContextBudget::for_model(1000, 200);
        b.begin_turn();
        assert!(!b.usage_seen());
        b.observe_usage(TokenUsage {
            input: 650,
            output: 10,
        });
        assert!(b.usage_seen());
        assert!(b.should_autocompact());

        let mut b2 = ContextBudget::for_model(1000, 200);
        b2.begin_turn();
        b2.observe_estimated(600);
        assert!(!b2.usage_seen(), "estimation ≠ signal réel");
        assert!(b2.should_microcompact());
        assert!(!b2.should_autocompact());
    }

    // US-030: after compaction, the 1st real usage becomes the baseline -> no
    // immediate double compaction; the threshold then measures growth.
    #[test]
    fn post_compaction_baseline_prevents_immediate_recompaction() {
        // window 1000, reserve 200 -> auto 640.
        let mut b = ContextBudget::for_model(1000, 200);
        b.mark_compacted(650);
        assert!(
            !b.should_autocompact(),
            "le baseline estimé bloque la recompaction avant usage réel"
        );
        // 1st real usage after compaction: 650 (incompressible overhead). Without a
        // baseline, 650 >= 640 -> immediate recompaction. With the baseline: 650 becomes
        // the prefill, so current - prefill = 0 -> no compaction.
        b.observe_usage(TokenUsage {
            input: 650,
            output: 5,
        });
        assert_eq!(b.prefill_input(), 650);
        assert!(
            !b.should_autocompact(),
            "le baseline neutralise l'overhead post-compaction"
        );
        // the conversation grows 640 above the baseline -> triggers again.
        b.observe_usage(TokenUsage {
            input: 650 + 640,
            output: 5,
        });
        assert!(b.should_autocompact(), "croissance réelle re-déclenche");
    }

    #[test]
    fn would_autocompact_projects_without_mutation() {
        let mut b = ContextBudget::for_model(1000, 200); // auto 640
        b.observe_usage(TokenUsage {
            input: 100,
            output: 5,
        });
        assert!(b.would_autocompact(640), "projection franchit le seuil");
        assert!(!b.would_autocompact(639));
        // the projection does not mutate the real budget.
        assert_eq!(b.current_input(), 100);
        assert!(!b.should_autocompact());
    }

    #[test]
    fn estimate_input_uses_counter() {
        let msgs = vec![Message::user("aaaaaaaa"), Message::assistant_text("bbbb")];
        let est = estimate_input(&msgs, &HeuristicCounter);
        // 8 bytes -> 2 tokens; 4 bytes -> 1 token
        assert_eq!(est, 3);
    }

    #[test]
    fn estimate_static_input_counts_system_context_and_tools() {
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "lit".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        }];
        let est = estimate_static_input(
            &Some("system prompt".into()),
            &[Message::user("ctxctxctxctx")],
            &tools,
            &HeuristicCounter,
        );
        assert!(
            est > estimate_input(&[Message::user("ctxctxctxctx")], &HeuristicCounter),
            "system et tools doivent compter dans l'estimation"
        );
    }
}
