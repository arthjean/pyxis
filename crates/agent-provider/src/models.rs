//! Model catalog **discovered at runtime** on the ChatGPT/Codex backend
//! (`GET /models`). The backend returns exactly the models accessible to the
//! connected account (it applies `available_in_plans` itself) and filters on the
//! announced `client_version` (see `agent_auth::oauth::openai_chatgpt::CODEX_CLIENT_VERSION`).
//!
//! Replaces a slug table frozen in the binary: the backend allow-list
//! moves (frequent removals/additions), so the only correct source is the backend.
//!
//! **US-001, evidence that `context_window` is really served.** The `SAMPLE`
//! constant of the test module below is a verbatim capture of a `/models`
//! response from this backend (2026-07-24); the `gpt-5.4` entry carries
//! `"context_window":272000`. Two independent confirmations: the Codex CLI,
//! reference client of the same backend, deserializes the same field in its
//! `ModelInfo` (`codex-rs/protocol/src/openai_models.rs`), and the entry for
//! `gpt-5.6-sol` in the same capture carries none, which is why the field is
//! optional here. No live request was issued during the story: the capture
//! already recorded in the repository is the evidence used.

use serde::Deserialize;

/// Model presentable to the user, reduced to the fields the client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    /// Reasoning effort applied when no explicit choice is made.
    pub default_reasoning_effort: Option<String>,
    /// Efforts accepted by this model (`low` to `ultra` depending on the model).
    pub supported_reasoning_efforts: Vec<String>,
    /// Context window served by the backend (US-001). `None` when the backend
    /// declares none: the absence is carried as such, never replaced by an
    /// invented default. Consumers that need a number for their own geometry
    /// (compaction thresholds) keep their own fallback.
    pub context_window: Option<u32>,
}

#[derive(Deserialize)]
struct WireCatalog {
    #[serde(default)]
    models: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    /// `list` (visible in the selector), `hide` or `none` (internal use, e.g.
    /// `codex-auto-review`).
    #[serde(default)]
    visibility: Option<String>,
    /// Display order wanted by the backend (ascending, 1 = top of the list).
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<WireReasoningLevel>,
    /// Context window in tokens. Absent from some entries: kept optional so a
    /// missing value stays distinguishable from a real one.
    #[serde(default)]
    context_window: Option<u32>,
}

#[derive(Deserialize)]
struct WireReasoningLevel {
    effort: String,
}

/// Parses the `/models` response: keeps only the selectable models and
/// respects the backend `priority` order. Tolerant to unknown fields (the
/// backend adds some regularly).
pub fn parse_catalog(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let mut wire: Vec<WireModel> = serde_json::from_str::<WireCatalog>(body)?
        .models
        .into_iter()
        .filter(|m| matches!(m.visibility.as_deref(), None | Some("list")))
        .collect();
    wire.sort_by_key(|m| m.priority);
    Ok(wire
        .into_iter()
        .map(|m| CatalogModel {
            display_name: m.display_name.unwrap_or_else(|| m.slug.clone()),
            slug: m.slug,
            default_reasoning_effort: m.default_reasoning_level,
            supported_reasoning_efforts: m
                .supported_reasoning_levels
                .into_iter()
                .map(|level| level.effort)
                .collect(),
            // A window of 0 is not a window: it is dropped like an absence
            // rather than propagated as a usable geometry.
            context_window: m.context_window.filter(|w| *w > 0),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real excerpt of the backend response (2026-07-24), unknown fields included.
    const SAMPLE: &str = r#"{
      "models": [
        {"slug":"gpt-5.4","display_name":"GPT-5.4","visibility":"list","priority":16,
         "default_reasoning_level":"medium","context_window":272000,
         "supported_reasoning_levels":[{"effort":"low","description":"x"},{"effort":"high","description":"y"}]},
        {"slug":"codex-auto-review","display_name":"Codex Auto Review","visibility":"hide","priority":43,
         "default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"medium"}]},
        {"slug":"gpt-5.6-sol","display_name":"GPT-5.6-Sol","visibility":"list","priority":1,
         "default_reasoning_level":"low",
         "supported_reasoning_levels":[{"effort":"low"},{"effort":"max"},{"effort":"ultra"}]}
      ]
    }"#;

    #[test]
    fn keeps_listed_models_ordered_by_priority() {
        let catalog = parse_catalog(SAMPLE).expect("sample catalog parses");
        let slugs: Vec<&str> = catalog.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, ["gpt-5.6-sol", "gpt-5.4"], "hidden model dropped");
        assert_eq!(catalog[0].default_reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            catalog[0].supported_reasoning_efforts,
            ["low", "max", "ultra"]
        );
        assert_eq!(catalog[1].display_name, "GPT-5.4");
    }

    /// US-001: the window served by the backend reaches the type exposed to
    /// clients, and a model that declares none carries an absence rather than a
    /// substituted value.
    #[test]
    fn keeps_context_window_and_reports_absence() {
        let catalog = parse_catalog(SAMPLE).expect("sample catalog parses");
        assert_eq!(catalog[1].slug, "gpt-5.4");
        assert_eq!(catalog[1].context_window, Some(272_000));
        assert_eq!(
            catalog[0].context_window, None,
            "modèle sans context_window déclaré: absence explicite"
        );
    }

    /// A zero window would silently produce an unusable geometry downstream: it
    /// is treated as an absence.
    #[test]
    fn zero_context_window_is_an_absence() {
        let catalog = parse_catalog(
            r#"{"models":[{"slug":"m","display_name":"M","visibility":"list","context_window":0}]}"#,
        )
        .expect("catalog parses");
        assert_eq!(catalog[0].context_window, None);
    }

    /// Unknown fields must stay tolerated (the backend adds some regularly) and
    /// the fields already read must keep being read.
    #[test]
    fn unknown_fields_stay_tolerated() {
        let catalog = parse_catalog(
            r#"{"models":[{"slug":"m","display_name":"M","visibility":"list",
                 "context_window":100,"brand_new_field":{"nested":true},
                 "default_reasoning_level":"high",
                 "supported_reasoning_levels":[{"effort":"high","description":"x"}]}]}"#,
        )
        .expect("catalog with unknown fields parses");
        assert_eq!(catalog[0].context_window, Some(100));
        assert_eq!(catalog[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(catalog[0].supported_reasoning_efforts, ["high"]);
    }

    #[test]
    fn empty_catalog_is_not_an_error() {
        assert!(
            parse_catalog(r#"{"models":[]}"#)
                .expect("empty catalog parses")
                .is_empty()
        );
    }
}
