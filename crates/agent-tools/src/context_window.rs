//! Context-window tools: `get_context_remaining` and `new_context_window`.
//! Ported from Codex (`codex-rs/core/src/tools/handlers/get_context_remaining.rs`,
//! `.../new_context_window.rs`).
//!
//! Both answer the same blind spot: the loop knows the context pressure and the
//! model does not. Without them a model either stops early "to be safe" on a
//! window that was three quarters empty, or keeps reading files until the loop
//! compacts under it and the plan it was holding disappears.
//!
//! Neither tool decides anything. `get_context_remaining` READS the snapshot the
//! loop published (`agent_core::budget::ContextWindowState`), and
//! `new_context_window` REQUESTS a compaction, which the loop grants at its next
//! safe point through the same arming a long `tool_result` uses (US-030 MidTurn).
//! A model therefore cannot rewrite a transcript on demand, nor compact between
//! a `tool_use` and its result.

use agent_core::budget::ContextWindow;
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ToolError;
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Reason shown when no turn has published a window yet. Stated rather than
/// guessed: a fabricated "100% free" is exactly the answer that makes a model
/// read another twenty files.
const NOT_PUBLISHED: &str = "The context budget has not been published yet (no model turn has completed \
     in this run). Proceed without a remaining-token figure.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetContextRemainingInput {}

/// Reports how much room is left before the loop compacts on its own.
pub struct GetContextRemaining;

#[async_trait]
impl Tool for GetContextRemaining {
    type Input = GetContextRemainingInput;

    fn name(&self) -> &str {
        "get_context_remaining"
    }
    fn description(&self) -> String {
        "Report how much context budget is left before this conversation is \
         automatically compacted. Call it before a large read or a long command \
         when the answer would change what you do: split the work, summarize \
         early, or ask for a fresh window."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// Numbers the loop itself computed: nothing external enters the context.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        CONTEXT_GUIDELINES
    }

    async fn call(&self, _input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let Some(window) = ctx.context_window.get() else {
            return Ok(ToolOutput::text(NOT_PUBLISHED));
        };
        Ok(
            ToolOutput::text(render(&window)).with_structured_content(serde_json::json!({
                "remaining_tokens": window.remaining_before_compaction(),
                "used_percent": window.used_percent(),
                "max_context_tokens": window.max_context,
                "measured": window.usage_seen,
            })),
        )
    }
}

fn render(window: &ContextWindow) -> String {
    let source = if window.usage_seen {
        "measured from backend usage"
    } else {
        "locally estimated (the backend reported no usage for this turn)"
    };
    format!(
        "About {} tokens left before automatic compaction ({}% of the compaction \
         budget used, window {} tokens, {source}).",
        window.remaining_before_compaction(),
        window.used_percent(),
        window.max_context,
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewContextWindowInput {
    /// What must survive the compaction. Not a parameter of the summary itself
    /// (the loop owns how it summarizes): it is what the model states, on the
    /// record, before the transcript shrinks under it.
    pub carry_over: String,
}

/// Asks for a fresh context window at the loop's next safe point.
pub struct NewContextWindow;

#[async_trait]
impl Tool for NewContextWindow {
    type Input = NewContextWindowInput;

    fn name(&self) -> &str {
        "new_context_window"
    }
    fn description(&self) -> String {
        "Request a fresh context window: the conversation is summarized at the \
         next safe point and the turn continues from that summary. Pass in \
         carry_over everything that must survive, because the raw transcript \
         will not. Use it when a long task is about to run out of room, not to \
         hide a mistake."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "carry_over": {
                    "type": "string",
                    "description": "Facts, decisions and remaining steps that must \
                                    survive the compaction."
                }
            },
            "required": ["carry_over"],
            "additionalProperties": false
        })
    }
    /// It mutates no file and starts no process, but it does change the shape of
    /// the conversation: not read-only, hence excluded from the parallel segment
    /// and refused in `Plan` mode.
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    /// Sensitive in the taint sense: a compaction requested right after
    /// untrusted content was read is a plausible way to drop the evidence of
    /// what that content did. Recent taint therefore forces a confirmation.
    fn is_sensitive(&self) -> bool {
        true
    }
    /// The acknowledgement is in-house; the carry-over is the model's own text.
    fn returns_untrusted(&self) -> bool {
        false
    }
    /// Nothing outside the conversation is touched, so the baseline is `Allow`:
    /// the taint rule and the permission mode still apply on top of it.
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        CONTEXT_GUIDELINES
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let carry_over = input.carry_over.trim();
        if carry_over.is_empty() {
            // Refused as a semantic error rather than a validation failure: the
            // model gets the reason and can retry in the same turn.
            return Ok(ToolOutput::error(
                "carry_over is empty: state what must survive the compaction, \
                 otherwise the fresh window starts from nothing.",
            ));
        }
        // The carry-over is echoed back so it exists in the transcript BEFORE the
        // compaction reads it. A summary built from a message the model never
        // sent would drop exactly what it asked to keep.
        Ok(ToolOutput::text(format!(
            "A fresh context window was requested; it takes effect before the \
             next model turn. Carried over:\n{carry_over}"
        ))
        .requesting_compaction())
    }
}

const CONTEXT_GUIDELINES: &[&str] = &[
    "get_context_remaining / new_context_window: check the remaining budget \
     before a large read, and request a fresh window only when a task genuinely \
     needs more room. A fresh window replaces the transcript with a summary: \
     anything not in carry_over is gone.",
];

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::budget::ContextWindowState;

    fn window() -> ContextWindow {
        ContextWindow {
            max_context: 200_000,
            output_reserve: 32_000,
            current_input: 90_000,
            auto_threshold: 134_400,
            prefill_input: 10_000,
            usage_seen: true,
        }
    }

    #[tokio::test]
    async fn an_unpublished_budget_is_reported_as_unknown_not_as_full() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = GetContextRemaining
            .call(GetContextRemainingInput {}, &ctx)
            .await
            .expect("reading an empty handle must not fail");
        assert!(
            out.content.contains("not been published"),
            "{}",
            out.content
        );
        assert!(
            out.structured_content.is_none(),
            "no figure must be published when none is known"
        );
    }

    #[tokio::test]
    async fn a_published_budget_is_reported_with_its_measurement_source() {
        let state = ContextWindowState::new();
        state.publish(window());
        let ctx = ToolCtx::new(std::env::temp_dir()).with_context_window(state);
        let out = GetContextRemaining
            .call(GetContextRemainingInput {}, &ctx)
            .await
            .expect("reading a published handle must succeed");
        // 134_400 - (90_000 - 10_000) = 54_400 tokens left.
        assert!(out.content.contains("54400"), "{}", out.content);
        assert!(out.content.contains("measured"), "{}", out.content);
        let structured = out
            .structured_content
            .expect("a published window must carry structured figures");
        assert_eq!(structured["remaining_tokens"], 54_400);
        assert_eq!(structured["used_percent"], 59);
    }

    #[test]
    fn a_budget_past_its_threshold_reports_zero_left_not_a_wrapped_number() {
        let over = ContextWindow {
            current_input: 400_000,
            ..window()
        };
        assert_eq!(over.remaining_before_compaction(), 0);
        assert_eq!(over.used_percent(), 100);
    }

    #[tokio::test]
    async fn a_fresh_window_request_arms_compaction_and_echoes_the_carry_over() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = NewContextWindow
            .call(
                NewContextWindowInput {
                    carry_over: "  the fix lives in registry.rs  ".to_string(),
                },
                &ctx,
            )
            .await
            .expect("a well-formed request must succeed");
        assert!(out.requests_compaction);
        assert!(out.content.contains("registry.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_carry_over_is_refused_without_arming_compaction() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = NewContextWindow
            .call(
                NewContextWindowInput {
                    carry_over: "   ".to_string(),
                },
                &ctx,
            )
            .await
            .expect("a refusal is a result, not a pipeline error");
        assert!(out.is_error);
        assert!(
            !out.requests_compaction,
            "a refused request must not compact anything"
        );
    }
}
