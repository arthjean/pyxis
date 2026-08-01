//! Immutable model-visible prompt captured for exactly one provider sampling.
//!
//! Priority under the non-history byte ceiling is deliberate: the effective
//! instructions and tool contract are indivisible, then ordered step context is
//! retained, then ephemeral control fragments. Dropping a lower-priority
//! fragment can remove context, but can never widen the advertised capabilities.

use agent_tokenizer::TokenCounter;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::budget::{estimate_input, estimate_static_input};
use crate::message::Message;
use crate::model::{ReasoningReplaySupport, ResolvedModelRuntime};
use crate::provider::CanonicalRequest;
use crate::step::ContextFragmentKind;
use crate::tools::StepToolPlan;

pub const MAX_NON_HISTORY_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_MODEL_SWITCH_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBaseline {
    pub profile_fingerprint: String,
    pub model_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_hash: Option<String>,
    pub instructions_fingerprint: String,
    pub project_context_fingerprint: String,
    pub skills_fingerprint: String,
    pub tool_plan_fingerprint: String,
}

impl ContextBaseline {
    pub fn validate(&self) -> Result<(), PromptSnapshotError> {
        for (field, fingerprint) in [
            ("profile_fingerprint", self.profile_fingerprint.as_str()),
            (
                "instructions_fingerprint",
                self.instructions_fingerprint.as_str(),
            ),
            (
                "project_context_fingerprint",
                self.project_context_fingerprint.as_str(),
            ),
            ("skills_fingerprint", self.skills_fingerprint.as_str()),
            ("tool_plan_fingerprint", self.tool_plan_fingerprint.as_str()),
        ] {
            if !is_sha256(fingerprint) {
                return Err(PromptSnapshotError::InvalidBaseline {
                    detail: format!("{field} is not a SHA-256 fingerprint"),
                });
            }
        }
        if self.model_slug.is_empty()
            || self.model_slug.len() > 256
            || self.model_slug.chars().any(char::is_control)
        {
            return Err(PromptSnapshotError::InvalidBaseline {
                detail: "model_slug must contain 1 to 256 bytes and no controls".into(),
            });
        }
        if self.comp_hash.as_ref().is_some_and(|hash| {
            hash.is_empty() || hash.len() > 256 || hash.chars().any(char::is_control)
        }) {
            return Err(PromptSnapshotError::InvalidBaseline {
                detail: "comp_hash must contain 1 to 256 bytes and no controls".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTransitionCause {
    Initial,
    LegacyResume,
    ModelChanged,
    InstructionsChanged,
    ProjectContextChanged,
    SkillsChanged,
    ToolPlanChanged,
    Compaction,
    CompHashChanged,
    OverloadFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTransition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ContextBaseline>,
    pub to: ContextBaseline,
    pub causes: Vec<ContextTransitionCause>,
}

impl ContextTransition {
    pub fn validate(&self) -> Result<(), PromptSnapshotError> {
        if let Some(from) = &self.from {
            from.validate()?;
        }
        self.to.validate()?;
        if self.causes.is_empty() {
            return Err(PromptSnapshotError::InvalidBaseline {
                detail: "context transition has no cause".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PromptSnapshot {
    model: String,
    model_runtime: Option<ResolvedModelRuntime>,
    reasoning_effort: Option<String>,
    reasoning_replay: bool,
    system: Option<String>,
    context_messages: Vec<Message>,
    messages: Vec<Message>,
    ephemeral_messages: Vec<Message>,
    tool_plan: StepToolPlan,
    max_output_tokens: u32,
    baseline: ContextBaseline,
    stable_prefix_fingerprint: String,
    fingerprint: String,
    diagnostics: Vec<String>,
}

impl PromptSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn capture(
        model: String,
        model_runtime: Option<ResolvedModelRuntime>,
        reasoning_effort: Option<String>,
        system: Option<String>,
        context_messages: Vec<Message>,
        context_kinds: Vec<ContextFragmentKind>,
        messages: &[Message],
        mut ephemeral_messages: Vec<Message>,
        tool_plan: StepToolPlan,
        max_output_tokens: u32,
        reasoning_replay: bool,
        model_switch_from: Option<&ContextBaseline>,
    ) -> Result<Self, PromptSnapshotError> {
        if let Some(previous) = model_switch_from {
            let fragment = bounded_model_switch(previous, model_runtime.as_ref());
            ephemeral_messages.insert(0, Message::user(fragment));
        }

        let fixed_bytes = serialized_len(&system).saturating_add(serialized_len(tool_plan.specs()));
        if fixed_bytes > MAX_NON_HISTORY_CONTEXT_BYTES {
            return Err(PromptSnapshotError::StaticContextTooLarge {
                bytes: fixed_bytes,
                max: MAX_NON_HISTORY_CONTEXT_BYTES,
            });
        }

        let mut diagnostics = Vec::new();
        let mut used = fixed_bytes;
        let (context_messages, context_kinds) = retain_context_within_budget(
            context_messages,
            context_kinds,
            &mut used,
            &mut diagnostics,
        );
        let ephemeral_messages = retain_within_budget(
            ephemeral_messages,
            &mut used,
            "ephemeral context",
            &mut diagnostics,
        );

        let profile_fingerprint = model_runtime
            .as_ref()
            .map(|runtime| runtime.fingerprint.clone())
            .unwrap_or_else(|| fingerprint(&(model.as_str(), reasoning_effort.as_deref())));
        let instructions_fingerprint = fingerprint(&system);
        let mut project_context = Vec::new();
        let mut skills = Vec::new();
        for (message, kind) in context_messages.iter().zip(&context_kinds) {
            match kind {
                ContextFragmentKind::Project => project_context.push(message),
                ContextFragmentKind::Skill => skills.push(message),
            }
        }
        let baseline = ContextBaseline {
            profile_fingerprint,
            model_slug: model.clone(),
            comp_hash: model_runtime
                .as_ref()
                .and_then(|runtime| runtime.comp_hash.clone()),
            instructions_fingerprint,
            project_context_fingerprint: fingerprint(&project_context),
            skills_fingerprint: fingerprint(&skills),
            tool_plan_fingerprint: tool_plan.fingerprint().to_string(),
        };
        baseline.validate()?;

        let stable_prefix_fingerprint = fingerprint(&(
            &baseline,
            &system,
            &context_messages,
            messages,
            tool_plan.specs(),
        ));
        let fingerprint = fingerprint(&(
            stable_prefix_fingerprint.as_str(),
            &ephemeral_messages,
            max_output_tokens,
            reasoning_replay,
        ));

        Ok(Self {
            model,
            model_runtime,
            reasoning_effort,
            reasoning_replay,
            system,
            context_messages,
            messages: messages.to_vec(),
            ephemeral_messages,
            tool_plan,
            max_output_tokens,
            baseline,
            stable_prefix_fingerprint,
            fingerprint,
            diagnostics,
        })
    }

    pub fn request(&self) -> CanonicalRequest {
        let mut messages = Vec::with_capacity(
            self.context_messages.len() + self.messages.len() + self.ephemeral_messages.len(),
        );
        messages.extend_from_slice(&self.context_messages);
        messages.extend_from_slice(&self.messages);
        messages.extend_from_slice(&self.ephemeral_messages);
        CanonicalRequest {
            model: self.model.clone(),
            model_runtime: self.model_runtime.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            reasoning_replay: self.reasoning_replay,
            system: self.system.clone(),
            messages,
            tools: self.tool_plan.specs().to_vec(),
            max_output_tokens: self.max_output_tokens,
            ..CanonicalRequest::default()
        }
    }

    pub fn static_input_tokens(&self, tokenizer: &dyn TokenCounter) -> u32 {
        estimate_static_input(
            &self.system,
            &self.context_messages,
            self.tool_plan.specs(),
            tokenizer,
        )
        .saturating_add(estimate_input(&self.ephemeral_messages, tokenizer))
    }

    pub fn baseline(&self) -> &ContextBaseline {
        &self.baseline
    }

    pub fn stable_prefix_fingerprint(&self) -> &str {
        &self.stable_prefix_fingerprint
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn tool_plan(&self) -> &StepToolPlan {
        &self.tool_plan
    }

    pub fn reasoning_replay(&self) -> bool {
        self.reasoning_replay
    }
}

pub fn transition_between(
    previous: Option<&ContextBaseline>,
    next: &ContextBaseline,
    mut additional: Vec<ContextTransitionCause>,
) -> Option<ContextTransition> {
    if previous == Some(next) && additional.is_empty() {
        return None;
    }
    match previous {
        None => additional.push(ContextTransitionCause::Initial),
        Some(previous) => {
            if previous.profile_fingerprint != next.profile_fingerprint {
                additional.push(ContextTransitionCause::ModelChanged);
            }
            if previous.instructions_fingerprint != next.instructions_fingerprint {
                additional.push(ContextTransitionCause::InstructionsChanged);
            }
            if previous.project_context_fingerprint != next.project_context_fingerprint {
                additional.push(ContextTransitionCause::ProjectContextChanged);
            }
            if previous.skills_fingerprint != next.skills_fingerprint {
                additional.push(ContextTransitionCause::SkillsChanged);
            }
            if previous.tool_plan_fingerprint != next.tool_plan_fingerprint {
                additional.push(ContextTransitionCause::ToolPlanChanged);
            }
            if previous.comp_hash != next.comp_hash {
                additional.push(ContextTransitionCause::CompHashChanged);
            }
        }
    }
    additional.sort_by_key(|cause| *cause as u8);
    additional.dedup();
    Some(ContextTransition {
        from: previous.cloned(),
        to: next.clone(),
        causes: additional,
    })
}

pub fn replay_enabled(runtime: Option<&ResolvedModelRuntime>) -> bool {
    runtime.is_some_and(|runtime| runtime.reasoning_replay == ReasoningReplaySupport::Enabled)
}

fn bounded_model_switch(
    previous: &ContextBaseline,
    runtime: Option<&ResolvedModelRuntime>,
) -> String {
    let next = runtime
        .map(|runtime| runtime.fingerprint.as_str())
        .unwrap_or("legacy");
    let fragment = format!(
        "<model_switch from_profile=\"{}\" to_profile=\"{}\">Use only the new model contract; do not replay prior encrypted reasoning.</model_switch>",
        previous.profile_fingerprint, next
    );
    if fragment.len() <= MAX_MODEL_SWITCH_BYTES {
        return fragment;
    }
    let mut cut = MAX_MODEL_SWITCH_BYTES;
    while cut > 0 && !fragment.is_char_boundary(cut) {
        cut -= 1;
    }
    fragment[..cut].to_string()
}

fn retain_within_budget(
    messages: Vec<Message>,
    used: &mut usize,
    label: &str,
    diagnostics: &mut Vec<String>,
) -> Vec<Message> {
    let mut retained = Vec::with_capacity(messages.len());
    let mut dropped = 0usize;
    for message in messages {
        let bytes = serialized_len(&message);
        if used.saturating_add(bytes) <= MAX_NON_HISTORY_CONTEXT_BYTES {
            *used += bytes;
            retained.push(message);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        diagnostics.push(format!(
            "{dropped} {label} message(s) omitted at the {MAX_NON_HISTORY_CONTEXT_BYTES}-byte boundary"
        ));
    }
    retained
}

fn retain_context_within_budget(
    messages: Vec<Message>,
    mut kinds: Vec<ContextFragmentKind>,
    used: &mut usize,
    diagnostics: &mut Vec<String>,
) -> (Vec<Message>, Vec<ContextFragmentKind>) {
    if kinds.len() != messages.len() {
        kinds = vec![ContextFragmentKind::Project; messages.len()];
    }
    let mut retained_messages = Vec::with_capacity(messages.len());
    let mut retained_kinds = Vec::with_capacity(messages.len());
    let mut dropped = 0usize;
    for (message, kind) in messages.into_iter().zip(kinds) {
        let bytes = serialized_len(&message);
        if used.saturating_add(bytes) <= MAX_NON_HISTORY_CONTEXT_BYTES {
            *used += bytes;
            retained_messages.push(message);
            retained_kinds.push(kind);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        diagnostics.push(format!(
            "{dropped} step context message(s) omitted at the {MAX_NON_HISTORY_CONTEXT_BYTES}-byte boundary"
        ));
    }
    (retained_messages, retained_kinds)
}

fn serialized_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn fingerprint<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptSnapshotError {
    #[error("fixed prompt context exceeds {max} bytes ({bytes})")]
    StaticContextTooLarge { bytes: usize, max: usize },
    #[error("invalid context baseline: {detail}")]
    InvalidBaseline { detail: String },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::provider::ToolSpec;
    use crate::tools::{
        ToolDispatch, ToolDispatchSnapshot, ToolEventSink, ToolInvocation, ToolOutcome,
    };

    use super::*;

    struct NoTools;

    #[async_trait::async_trait]
    impl ToolDispatch for NoTools {
        async fn dispatch(
            &self,
            _calls: Vec<ToolInvocation>,
            _events: ToolEventSink,
        ) -> Vec<ToolOutcome> {
            Vec::new()
        }
    }

    fn plan() -> StepToolPlan {
        let spec = ToolSpec::function(
            "read",
            "read a file",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        );
        StepToolPlan::capture(
            ToolDispatchSnapshot::new(1, vec![spec], Arc::new(NoTools)),
            false,
        )
    }

    fn capture(history: &[Message], context: Vec<Message>) -> PromptSnapshot {
        PromptSnapshot::capture(
            "model".into(),
            None,
            None,
            Some("instructions".into()),
            context.clone(),
            vec![ContextFragmentKind::Project; context.len()],
            history,
            Vec::new(),
            plan(),
            1_024,
            false,
            None,
        )
        .expect("snapshot")
    }

    #[test]
    fn identical_sources_and_history_keep_the_stable_prefix() {
        let history = vec![Message::user("task")];
        let first = capture(&history, vec![Message::user("project")]);
        let second = capture(&history, vec![Message::user("project")]);
        assert_eq!(
            first.stable_prefix_fingerprint(),
            second.stable_prefix_fingerprint()
        );
        assert_eq!(first.baseline(), second.baseline());
    }

    #[test]
    fn provider_request_is_a_copy_of_the_canonical_history() {
        let history = vec![Message::user("canonical")];
        let snapshot = capture(&history, Vec::new());
        let mut request = snapshot.request();
        request.messages[0] = Message::user("normalized");
        assert_eq!(history, vec![Message::user("canonical")]);
        assert_eq!(snapshot.request().messages, history);
    }

    #[test]
    fn non_history_context_is_bounded_without_dropping_tools() {
        let context = vec![
            Message::user("a".repeat(MAX_NON_HISTORY_CONTEXT_BYTES)),
            Message::user("tail"),
        ];
        let snapshot = capture(&[Message::user("history")], context);
        assert!(!snapshot.diagnostics().is_empty());
        assert_eq!(snapshot.request().tools.len(), 1);
        assert_eq!(
            snapshot.request().messages.last(),
            Some(&Message::user("history"))
        );
    }

    #[test]
    fn transitions_are_suppressed_until_a_baseline_identity_moves() {
        let first = capture(&[Message::user("task")], Vec::new());
        assert!(transition_between(Some(first.baseline()), first.baseline(), Vec::new()).is_none());
        let second = capture(
            &[Message::user("task")],
            vec![Message::user("changed project")],
        );
        let transition = transition_between(Some(first.baseline()), second.baseline(), Vec::new())
            .expect("context moved");
        assert!(
            transition
                .causes
                .contains(&ContextTransitionCause::ProjectContextChanged)
        );
    }

    #[test]
    fn baseline_separates_project_and_skill_fragments() {
        let context = vec![Message::user("same visible bytes")];
        let project = PromptSnapshot::capture(
            "model".into(),
            None,
            None,
            Some("instructions".into()),
            context.clone(),
            vec![ContextFragmentKind::Project],
            &[Message::user("task")],
            Vec::new(),
            plan(),
            1_024,
            false,
            None,
        )
        .unwrap();
        let skill = PromptSnapshot::capture(
            "model".into(),
            None,
            None,
            Some("instructions".into()),
            context,
            vec![ContextFragmentKind::Skill],
            &[Message::user("task")],
            Vec::new(),
            plan(),
            1_024,
            false,
            None,
        )
        .unwrap();
        let transition =
            transition_between(Some(project.baseline()), skill.baseline(), Vec::new()).unwrap();
        assert!(
            transition
                .causes
                .contains(&ContextTransitionCause::ProjectContextChanged)
        );
        assert!(
            transition
                .causes
                .contains(&ContextTransitionCause::SkillsChanged)
        );
    }
}
