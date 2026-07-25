//! Selection of the system prompt calibrated on the model slug (US-027). The
//! templates are embedded through `include_str!` (Codex CLI pattern): a LONG
//! `gpt_5_2`-style prompt for the generic GPT-5.x models (the behavioral spec
//! is not in their weights), a SHORT prompt for the `*-codex` fine-tunes.
//!
//! The slug can change in session through `/models` -> `select_system_prompt` is
//! called again on every turn (see `interactive::launch_turn`), not frozen at startup.

/// Long prompt (generic GPT-5.x models): AGENTS.md spec sections, Autonomy &
/// persistence, Responsiveness/preamble, editing guidance (anti re-reading).
const GPT5_GENERIC: &str = include_str!("../prompts/gpt5_generic.md");

/// Short prompt (Codex `*-codex` fine-tuned models): the spec is in the weights.
const CODEX_FINETUNED: &str = include_str!("../prompts/codex_finetuned.md");

/// Selects the template from the slug. A Codex fine-tuned slug (containing
/// `-codex`) gets the short prompt; any other slug, generic `gpt-5.*` or
/// UNKNOWN, gets the long prompt (safe default: better to over-specify than
/// to leave a generic model without a scaffold).
pub fn select_system_prompt(slug: &str) -> &'static str {
    if uses_codex_finetuned_prompt(slug) {
        CODEX_FINETUNED
    } else {
        GPT5_GENERIC
    }
}

pub fn uses_codex_finetuned_prompt(slug: &str) -> bool {
    slug.to_ascii_lowercase().contains("-codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section present ONLY in the long prompt (discriminating marker).
    const LONG_ONLY: &str = "## AGENTS.md Specification";

    #[test]
    fn generic_slug_gets_long_prompt() {
        let p = select_system_prompt("gpt-5.5");
        assert!(p.contains(LONG_ONLY), "long prompt expected");
        assert!(p.contains("## Autonomy and Persistence"));
        assert!(p.contains("Do NOT reread") && p.contains("confirmed success"));
    }

    #[test]
    fn codex_finetuned_slug_gets_short_prompt() {
        let p = select_system_prompt("gpt-5-codex");
        assert!(
            !p.contains(LONG_ONLY),
            "short prompt should omit long sections"
        );
        assert!(p.contains("Be autonomous"));
        assert!(!select_system_prompt("gpt-5.2-codex-max").contains(LONG_ONLY));
    }

    #[test]
    fn unknown_slug_falls_back_to_long_prompt() {
        assert!(select_system_prompt("some-future-model").contains(LONG_ONLY));
        assert!(select_system_prompt("").contains(LONG_ONLY));
    }

    #[test]
    fn short_prompt_is_actually_shorter() {
        assert!(CODEX_FINETUNED.len() < GPT5_GENERIC.len());
    }
}
