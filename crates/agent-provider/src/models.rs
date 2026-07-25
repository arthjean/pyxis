//! Model catalog **discovered at runtime** on the ChatGPT/Codex backend
//! (`GET /models`). The backend returns exactly the models accessible to the
//! connected account (it applies `available_in_plans` itself) and filters on the
//! announced `client_version` (see `agent_auth::oauth::openai_chatgpt::CODEX_CLIENT_VERSION`).
//!
//! Replaces a slug table frozen in the binary: the backend allow-list
//! moves (frequent removals/additions), so the only correct source is the backend.

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

    #[test]
    fn empty_catalog_is_not_an_error() {
        assert!(
            parse_catalog(r#"{"models":[]}"#)
                .expect("empty catalog parses")
                .is_empty()
        );
    }
}
