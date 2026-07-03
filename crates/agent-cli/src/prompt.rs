//! Sélection du system prompt calibré selon le slug du modèle (US-027). Les
//! templates sont embarqués via `include_str!` (pattern Codex CLI) : un prompt
//! LONG type `gpt_5_2` pour les modèles GPT-5.x génériques (la spec comportementale
//! n'est pas dans leurs poids), un prompt COURT pour les fine-tunés `*-codex`.
//!
//! Le slug peut changer en session via `/models` → `select_system_prompt` est
//! rappelé à chaque tour (cf. `interactive::launch_turn`), pas figé au démarrage.

/// Prompt long (modèles génériques GPT-5.x) : sections AGENTS.md spec, Autonomie &
/// persistance, Réactivité/préambule, guidance d'édition (anti-relecture).
const GPT5_GENERIC: &str = include_str!("../prompts/gpt5_generic.md");

/// Prompt court (modèles fine-tunés Codex `*-codex`) : la spec est dans les poids.
const CODEX_FINETUNED: &str = include_str!("../prompts/codex_finetuned.md");

/// Sélectionne le template selon le slug. Un slug fine-tuné Codex (contient
/// `-codex`) reçoit le prompt court ; tout autre slug — générique `gpt-5.*` ou
/// INCONNU — reçoit le prompt long (défaut sûr : mieux vaut sur-spécifier que
/// laisser un modèle générique sans scaffold).
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

    /// Section présente UNIQUEMENT dans le prompt long (marqueur discriminant).
    const LONG_ONLY: &str = "## Spécification AGENTS.md";

    #[test]
    fn generic_slug_gets_long_prompt() {
        let p = select_system_prompt("gpt-5.5");
        assert!(p.contains(LONG_ONLY), "prompt long attendu");
        assert!(p.contains("## Autonomie et persistance"));
        // instruction anti-relecture présente (AC).
        assert!(p.contains("Ne relis PAS") && p.contains("réussi"));
    }

    #[test]
    fn codex_finetuned_slug_gets_short_prompt() {
        let p = select_system_prompt("gpt-5-codex");
        assert!(
            !p.contains(LONG_ONLY),
            "prompt court (pas les sections longues)"
        );
        assert!(p.contains("Sois autonome"));
        // autre variante `-codex`.
        assert!(!select_system_prompt("gpt-5.2-codex-max").contains(LONG_ONLY));
    }

    #[test]
    fn unknown_slug_falls_back_to_long_prompt() {
        // défaut sûr : un slug inconnu reçoit le prompt long.
        assert!(select_system_prompt("some-future-model").contains(LONG_ONLY));
        assert!(select_system_prompt("").contains(LONG_ONLY));
    }

    #[test]
    fn short_prompt_is_actually_shorter() {
        assert!(CODEX_FINETUNED.len() < GPT5_GENERIC.len());
    }
}
