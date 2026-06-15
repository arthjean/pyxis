//! `ContextBudget` — calculé UNE FOIS par modèle (invariant 5), source unique de
//! vérité pour micro/auto-compaction. Lit l'`usage` du stream si présent, sinon
//! retombe sur le tokenizer local (provider sans `usage` en stream, ARCHITECTURE
//! §3.3 / PROVIDERS §4.3 ; sert aussi l'estimation pré-tour US-014).

use agent_tokenizer::TokenCounter;

use crate::message::{ContentBlock, Message};
use crate::provider::TokenUsage;

#[derive(Debug, Clone)]
pub struct ContextBudget {
    max_context: u32,
    output_reserve: u32,
    micro_threshold: u32,
    auto_threshold: u32,
    current_input: u32,
    usage_seen: bool,
}

impl ContextBudget {
    /// Construit le budget depuis la fenêtre du modèle. Seuils : micro à 70 %,
    /// auto à 80 % de la fenêtre utilisable (`max_context - output_reserve`).
    pub fn for_model(max_context: u32, output_reserve: u32) -> Self {
        let usable = max_context.saturating_sub(output_reserve);
        Self {
            max_context,
            output_reserve,
            micro_threshold: pct(usable, 70),
            auto_threshold: pct(usable, 80),
            current_input: 0,
            usage_seen: false,
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

    /// Nouveau tour : on remet le flag `usage_seen` (le compte courant, lui,
    /// reflète l'état du contexte et n'est pas remis).
    pub fn begin_turn(&mut self) {
        self.usage_seen = false;
    }

    /// Chemin nominal : consomme l'`usage` émis par le stream.
    pub fn observe_usage(&mut self, usage: TokenUsage) {
        self.current_input = usage.input;
        self.usage_seen = true;
    }

    /// Fallback (provider sans usage) : alimente le seuil avec une estimation
    /// locale. NE met PAS `usage_seen` (c'est une estimation, pas un signal réel).
    pub fn observe_estimated(&mut self, estimated_input: u32) {
        self.current_input = estimated_input;
    }

    pub fn should_microcompact(&self) -> bool {
        self.current_input >= self.micro_threshold
    }
    pub fn should_autocompact(&self) -> bool {
        self.current_input >= self.auto_threshold
    }
}

fn pct(v: u32, p: u32) -> u32 {
    ((u64::from(v) * u64::from(p)) / 100) as u32
}

/// Estime les tokens d'entrée d'un transcript via un `TokenCounter` (fallback
/// quand l'`usage` n'est pas fourni). Approxime les images à 0.
pub fn estimate_input(messages: &[Message], counter: &dyn TokenCounter) -> u32 {
    let mut total = 0usize;
    for m in messages {
        for b in &m.content {
            total += match b {
                ContentBlock::Text { text } | ContentBlock::Thinking { text } => {
                    counter.count_text(text)
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    counter.count_text(name) + counter.count_text(&input.to_string())
                }
                ContentBlock::ToolResult { content, .. } => counter.count_text(content),
                ContentBlock::Image { .. } => 0,
            };
        }
    }
    u32::try_from(total).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tokenizer::HeuristicCounter;

    #[test]
    fn budget_thresholds_from_single_source() {
        // fenêtre 1000, réserve 200 → utilisable 800 ; micro 560, auto 640.
        let b = ContextBudget::for_model(1000, 200);
        assert_eq!(b.output_reserve(), 200);
        assert_eq!(b.micro_threshold(), 560);
        assert_eq!(b.auto_threshold(), 640);
        assert!(!b.should_microcompact());
        assert!(!b.should_autocompact());
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

    #[test]
    fn estimate_input_uses_counter() {
        let msgs = vec![Message::user("aaaaaaaa"), Message::assistant_text("bbbb")];
        let est = estimate_input(&msgs, &HeuristicCounter);
        // 8 octets → 2 tokens ; 4 octets → 1 token
        assert_eq!(est, 3);
    }
}
