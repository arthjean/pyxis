//! The system prompt selected by the effective model runtime.
//!
//! The instructions almost always come from the REMOTE catalog
//! (`https://chatgpt.com/backend-api/codex/models`, see
//! `agent_provider::models::ModelCatalog::catalog_model`), so they describe the
//! Codex harness: `commentary`/`final` channels, `skills.list`/`skills.read`,
//! and tools reached by names Pyxis does not always expose the same way. The
//! bundled `prompts/*.md` only answer when that catalog is unreachable.
//!
//! A model strong enough to reconcile the two reconciles them; a smaller one
//! reads the gap as a missing capability and asks the user for the repository
//! instead of opening it. So the selected prompt is CLOSED with a harness
//! section that states what this harness actually is. It is appended, never
//! substituted: the upstream instructions carry the model's own training, and
//! only the contradictions need answering.

use agent_core::model::ResolvedModelRuntime;

/// True of every harness, whichever prompt was selected.
const HARNESS: &str = "\n\n\
# Pyxis harness contract\n\n\
This section describes the harness you are ACTUALLY running in. Anything above \
that contradicts it was written for a different harness; this section wins.\n\n\
- You run in Pyxis, a terminal coding agent. There is no `commentary` channel \
and no `final` channel: everything you write reaches the user as one stream.\n\
- The `<environment>` message states the working directory and what the \
filesystem grants you. That access is real and immediate. You can read, search \
and edit the workspace yourself, so never answer that you have no access to the \
repository, and never ask the user to paste files you can open.\n\
- A question about the workspace (\"what do you think of this project?\", \"how \
does X work?\", \"is this safe?\") authorizes read-only exploration on the spot. \
Explore first, then answer with evidence. Ask the user only when a genuine \
choice would change the result, never to decide where to start.\n\
- Skills, when any exist, are listed in the project context. `skills.list` and \
`skills.read` do not exist here: a skill is a file you open with your own \
tools.\n\
- Edits go through `apply_patch`, or through the `write`/`edit` pair. Both \
contracts are live; pick one and stay with it for a given file.";

/// Only true of a `code_mode_only` model, where the direct surface is `exec`
/// and `wait` alone (see `crate::runtime::CliStepSource::compose_tool_plan`).
/// Without this, the upstream instruction to "use `apply_patch`" names a tool
/// the model cannot find in its contract.
const CODE_MODE_ONLY: &str = "\n\
- You orchestrate through `exec` only. `exec` and `wait` are the sole tools you \
call directly; every other tool, `apply_patch` and `exec_command` included, is \
reached from JavaScript inside an `exec` cell, on the `tools` object (for \
example `await tools.read({ path: \"README.md\" })`). The `exec` tool \
description lists the exact signatures. A tool missing from your direct \
contract is not a missing capability: it is one cell away.";

/// The instructions of `runtime`, closed by the harness contract.
pub fn select_system_prompt(runtime: &ResolvedModelRuntime) -> String {
    let mut prompt =
        String::with_capacity(runtime.instructions.len() + HARNESS.len() + CODE_MODE_ONLY.len());
    prompt.push_str(&runtime.instructions);
    prompt.push_str(HARNESS);
    if runtime.tool_mode.hides_nested_tools() {
        prompt.push_str(CODE_MODE_ONLY);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::model::{
        InputModality, ModelRetryPolicy, ModelRuntimeSource, ModelToolCapabilities, ModelToolMode,
        MultiAgentVersion, ReasoningReplaySupport, ResponsesDialect, TruncationMode,
        TruncationPolicy,
    };

    fn runtime(tool_mode: ModelToolMode) -> ResolvedModelRuntime {
        ResolvedModelRuntime {
            slug: "gpt-5.6-luna".into(),
            source: ModelRuntimeSource::Embedded {
                version: "test".into(),
            },
            instructions: "You are Codex, an agent based on GPT-5.".into(),
            fingerprint: "a".repeat(64),
            context_window: 10_000,
            auto_compact_token_limit: 8_000,
            input_modalities: vec![InputModality::Text],
            reasoning_effort: None,
            supports_verbosity: false,
            verbosity: None,
            supports_parallel_tool_calls: false,
            tool_capabilities: ModelToolCapabilities::default(),
            service_tiers: Vec::new(),
            reasoning_replay: ReasoningReplaySupport::Disabled,
            responses_dialect: ResponsesDialect::Lite,
            tool_mode,
            multi_agent_version: MultiAgentVersion::Disabled,
            truncation: TruncationPolicy {
                mode: TruncationMode::Tokens,
                limit: 1_000,
            },
            retry: ModelRetryPolicy {
                max_attempts: 2,
                backoff_base_ms: 50,
            },
            max_output_tokens: 100,
            comp_hash: None,
        }
    }

    /// The upstream instructions are kept whole: they carry the model's own
    /// training and only their contradictions are answered.
    #[test]
    fn the_selected_instructions_are_appended_to_never_replaced() {
        let prompt = select_system_prompt(&runtime(ModelToolMode::Direct));
        assert!(prompt.starts_with("You are Codex, an agent based on GPT-5."));
        assert!(prompt.contains("# Pyxis harness contract"));
    }

    /// The regression this section exists for: a model answering that it cannot
    /// reach the repository, on a harness that hands it the whole workspace.
    #[test]
    fn every_model_is_told_the_filesystem_access_is_real() {
        for mode in [
            ModelToolMode::Direct,
            ModelToolMode::CodeMode,
            ModelToolMode::CodeModeOnly,
        ] {
            let prompt = select_system_prompt(&runtime(mode));
            assert!(
                prompt.contains("never answer that you have no access to the repository"),
                "{mode:?}"
            );
        }
    }

    /// `apply_patch` is named by the upstream instructions as a direct call. On
    /// a `code_mode_only` model it is reachable from a cell only, and nowhere
    /// else does the model learn that.
    #[test]
    fn only_a_code_mode_only_model_is_told_where_its_tools_live() {
        let direct = select_system_prompt(&runtime(ModelToolMode::Direct));
        assert!(!direct.contains("You orchestrate through `exec` only"));

        let orchestrating = select_system_prompt(&runtime(ModelToolMode::CodeModeOnly));
        assert!(orchestrating.contains("You orchestrate through `exec` only"));
        assert!(orchestrating.contains("tools.read"));
    }
}
