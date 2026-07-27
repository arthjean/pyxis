//! `update_plan` tool (US-009): the model publishes the plan of the current
//! task and keeps it up to date. Ported from Codex
//! (`codex-rs/core/src/tools/handlers/plan_spec.rs:7-58`): same item shape
//! (`{ step, status }`), same three states, same invariant of AT MOST ONE
//! `in_progress`.
//!
//! The plan is addressed to the HUMAN, not to the loop: it changes nothing in
//! the agent's decisions, travels as a dispatch event (`ToolOutput::plan`), and
//! a client that ignores the variant keeps its previous behavior. The model
//! only gets a short acknowledgement back.

use agent_core::event::{PlanStatus, PlanStep, PlanView};
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Bounds on the plan. A plan is a summary; past these sizes it stops being one
/// and starts costing context for nothing.
const MAX_STEPS: usize = 32;
const MAX_STEP_BYTES: usize = 400;
const MAX_EXPLANATION_BYTES: usize = 2_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanInput {
    /// Optional rationale for this update.
    #[serde(default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItemInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItemInput {
    pub step: String,
    pub status: PlanStatus,
}

pub struct UpdatePlan;

#[async_trait]
impl Tool for UpdatePlan {
    type Input = UpdatePlanInput;

    fn name(&self) -> &str {
        "update_plan"
    }
    fn description(&self) -> String {
        "Publish or update the plan of the current task. Each item carries a \
         step and a status among pending, in_progress and completed; AT MOST \
         ONE step may be in_progress. Send the WHOLE plan on every update, not \
         only the item that changed. Parameters: plan (list of items), \
         explanation (optional)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": ["string", "null"],
                    "description": "Why the plan changes, or null."
                },
                "plan": {
                    "type": "array",
                    "description": "Complete list of the steps, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": { "type": "string", "description": "Description of the step." },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "State of the step."
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["explanation", "plan"],
            "additionalProperties": false
        })
    }
    /// No disk, no process, no network: several plans in the same batch are
    /// harmless, and nothing here justifies a confirmation.
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// In-house acknowledgement, and a plan the model wrote itself: nothing
    /// external enters the context.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        PLAN_GUIDELINES
    }

    /// AC2: an invalid plan is refused HERE, before execution. The refusal
    /// becomes an error result the model reads and can correct, which leaves
    /// the turn intact.
    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_plan(input)
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let view = PlanView {
            explanation: input.explanation.filter(|e| !e.trim().is_empty()),
            steps: input
                .plan
                .into_iter()
                .map(|item| PlanStep {
                    step: item.step,
                    status: item.status,
                })
                .collect(),
        };
        let done = view
            .steps
            .iter()
            .filter(|s| s.status == PlanStatus::Completed)
            .count();
        let total = view.steps.len();
        Ok(
            ToolOutput::text(format!("Plan updated: {done}/{total} steps completed."))
                .with_plan(view),
        )
    }
}

const PLAN_GUIDELINES: &[&str] = &[
    "update_plan: send the COMPLETE plan on every call, with at most one step \
     in_progress. Update it when a step actually changes state, not on every \
     message.",
];

/// Pure validation of a plan (US-009 AC1/AC2), testable without a context.
fn validate_plan(input: &UpdatePlanInput) -> Result<(), ValidationError> {
    if input.plan.is_empty() {
        return Err(ValidationError::new(
            "empty plan: send at least one step, or do not call update_plan",
        ));
    }
    if input.plan.len() > MAX_STEPS {
        return Err(ValidationError::new(format!(
            "too many steps: {} > {MAX_STEPS}",
            input.plan.len()
        )));
    }
    if let Some(explanation) = &input.explanation
        && explanation.len() > MAX_EXPLANATION_BYTES
    {
        return Err(ValidationError::new(format!(
            "explanation too large: {} bytes > {MAX_EXPLANATION_BYTES}",
            explanation.len()
        )));
    }
    for (i, item) in input.plan.iter().enumerate() {
        if item.step.trim().is_empty() {
            return Err(ValidationError::new(format!("step {} is empty", i + 1)));
        }
        if item.step.len() > MAX_STEP_BYTES {
            return Err(ValidationError::new(format!(
                "step {} too large: {} bytes > {MAX_STEP_BYTES}",
                i + 1,
                item.step.len()
            )));
        }
    }
    let in_progress = input
        .plan
        .iter()
        .filter(|i| i.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(ValidationError::new(format!(
            "{in_progress} steps are in_progress: at most one step may be \
             in_progress; mark the others pending or completed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(step: &str, status: PlanStatus) -> PlanItemInput {
        PlanItemInput {
            step: step.to_string(),
            status,
        }
    }

    #[test]
    fn single_in_progress_is_accepted() {
        let input = UpdatePlanInput {
            explanation: None,
            plan: vec![
                item("read", PlanStatus::Completed),
                item("write", PlanStatus::InProgress),
                item("test", PlanStatus::Pending),
            ],
        };
        assert!(validate_plan(&input).is_ok());
    }

    #[test]
    fn two_in_progress_are_refused_and_the_reason_names_the_constraint() {
        let input = UpdatePlanInput {
            explanation: None,
            plan: vec![
                item("a", PlanStatus::InProgress),
                item("b", PlanStatus::InProgress),
            ],
        };
        let err = validate_plan(&input).expect_err("two in_progress steps must be refused");
        assert!(
            err.to_string().contains("at most one step"),
            "the refusal must name the constraint: {err}"
        );
    }

    #[test]
    fn empty_plan_is_refused() {
        let input = UpdatePlanInput {
            explanation: None,
            plan: Vec::new(),
        };
        assert!(validate_plan(&input).is_err());
    }

    #[test]
    fn empty_step_is_refused_by_position() {
        let input = UpdatePlanInput {
            explanation: None,
            plan: vec![
                item("ok", PlanStatus::Pending),
                item("  ", PlanStatus::Pending),
            ],
        };
        let err = validate_plan(&input).expect_err("an empty step must be refused");
        assert!(err.to_string().contains("step 2"), "{err}");
    }

    #[test]
    fn status_deserializes_from_the_codex_wire_names() {
        let input: UpdatePlanInput = serde_json::from_value(serde_json::json!({
            "explanation": null,
            "plan": [
                { "step": "a", "status": "pending" },
                { "step": "b", "status": "in_progress" },
                { "step": "c", "status": "completed" }
            ]
        }))
        .expect("the Codex wire names must parse");
        assert_eq!(input.plan[1].status, PlanStatus::InProgress);
    }

    #[tokio::test]
    async fn a_valid_plan_travels_on_the_output() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = UpdatePlan
            .call(
                UpdatePlanInput {
                    explanation: Some("  ".to_string()),
                    plan: vec![
                        item("a", PlanStatus::Completed),
                        item("b", PlanStatus::InProgress),
                    ],
                },
                &ctx,
            )
            .await
            .expect("a valid plan must succeed");
        let plan = out.plan.expect("the plan must travel with the output");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(
            plan.explanation, None,
            "a blank explanation is dropped rather than displayed empty"
        );
        assert!(out.content.contains("1/2"), "{}", out.content);
    }
}
