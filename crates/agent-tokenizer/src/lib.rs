//! `agent-tokenizer`: local token counting. Headless (no TUI/HTTP
//! dependency). Indispensable to the `ContextBudget` fallback when the provider
//! emits no `usage` in the stream (see ARCHITECTURE 3.3 / PROVIDERS 4.3) and to
//! the pre-turn budget estimate (US-014).
//!
//! The default is a **heuristic** (roughly 1 token / 4 bytes): enough for a
//! compaction *threshold* (we do not need the exact count, only a monotonic
//! signal). An exact tiktoken-rs counter is available behind the `tiktoken`
//! feature.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Counts tokens from raw text. `Send + Sync` so it can be injected
/// as a `dyn TokenCounter` into the `Deps` of `agent-core`.
pub trait TokenCounter: Send + Sync {
    /// Estimates the number of tokens of a text fragment.
    fn count_text(&self, text: &str) -> usize;
}

/// Dependency-free heuristic: ~1 token per 4 UTF-8 bytes, floored at 1 when
/// non-empty. Deliberately conservative (slightly overestimates) to trigger
/// compaction *before* the real limit rather than after.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn count_text(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        // div_ceil: any non-empty text is worth at least 1 token.
        text.len().div_ceil(4)
    }
}

/// Exact counter based on tiktoken-rs (BPE `cl100k_base`/`o200k_base`).
/// Available behind the `tiktoken` feature. For non-OpenAI models, it is
/// a reasonable approximation (better than the heuristic) of the threshold.
#[cfg(feature = "tiktoken")]
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
}

#[cfg(feature = "tiktoken")]
impl TiktokenCounter {
    /// Builds an `o200k_base` counter (recent models). Fallible: falls back
    /// on the heuristic when init fails on the caller side.
    pub fn o200k() -> Result<Self, anyhow::Error> {
        Ok(Self {
            bpe: tiktoken_rs::o200k_base()?,
        })
    }
}

#[cfg(feature = "tiktoken")]
impl TokenCounter for TiktokenCounter {
    fn count_text(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_is_monotone_and_handles_empty() {
        let c = HeuristicCounter;
        assert_eq!(c.count_text(""), 0);
        assert_eq!(c.count_text("a"), 1);
        assert_eq!(c.count_text("abcd"), 1);
        assert_eq!(c.count_text("abcde"), 2);
        // monotonic: more text means at least as many tokens
        assert!(c.count_text("hello world hello world") > c.count_text("hello"));
    }

    #[test]
    fn heuristic_is_object_safe() {
        let c: Box<dyn TokenCounter> = Box::new(HeuristicCounter);
        assert!(c.count_text("some text") >= 1);
    }
}
