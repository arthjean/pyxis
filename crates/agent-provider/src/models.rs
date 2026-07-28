//! Rich model catalog and deterministic runtime resolution.
//!
//! The remote `/models` payload wins only as a complete descriptor. A malformed
//! refresh leaves the last valid catalog installed; a partial entry falls back
//! as a whole to the versioned embedded entry. Explicit unknown required
//! capabilities stay incompatible rather than degrading to direct tools.

use std::collections::HashMap;

use agent_core::model::{
    InputModality, ModelDescriptor, ModelRetryPolicy, ModelRuntimeError, ModelRuntimeSource,
    ModelToolMode, ReasoningReplaySupport, ResolvedModelRuntime, ResponsesDialect, TruncationMode,
    TruncationPolicy,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const EMBEDDED_CATALOG_VERSION: &str = "2026-07-28.codex-8e271dc02b23";
pub const MODELS_ENDPOINT: &str = "/backend-api/codex/models";
const MAX_CATALOG_MODELS: usize = 256;

const GENERIC_INSTRUCTIONS: &str = include_str!("../../agent-cli/prompts/gpt5_generic.md");
const CODEX_INSTRUCTIONS: &str = include_str!("../../agent-cli/prompts/codex_finetuned.md");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub context_window: Option<u32>,
    pub source: ModelRuntimeSource,
    pub incompatibility_reason: Option<String>,
}

#[derive(Debug, Clone)]
enum RemoteEntry {
    Complete {
        descriptor: ModelDescriptor,
        source: ModelRuntimeSource,
    },
    Partial {
        display_name: String,
        reason: String,
        source: ModelRuntimeSource,
    },
    Incompatible {
        display_name: String,
        reason: String,
        source: ModelRuntimeSource,
    },
}

#[derive(Debug, Clone)]
struct RemoteCatalog {
    ordered_slugs: Vec<String>,
    entries: HashMap<String, RemoteEntry>,
}

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    embedded: HashMap<String, ModelDescriptor>,
    embedded_order: Vec<String>,
    remote: Option<RemoteCatalog>,
    diagnostics: Vec<String>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::embedded()
    }
}

impl ModelCatalog {
    pub fn embedded() -> Self {
        let descriptors = embedded_descriptors();
        let embedded_order = descriptors
            .iter()
            .map(|descriptor| descriptor.slug.clone())
            .collect();
        let embedded = descriptors
            .into_iter()
            .map(|descriptor| (descriptor.slug.clone(), descriptor))
            .collect();
        Self {
            embedded,
            embedded_order,
            remote: None,
            diagnostics: Vec::new(),
        }
    }

    /// Installs one complete remote snapshot. Errors and empty snapshots leave
    /// the previous valid state untouched.
    pub fn install_remote(
        &mut self,
        body: &str,
        fetched_at: &str,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let wire: WireCatalog = serde_json::from_str(body)
            .map_err(|error| CatalogError::Malformed(error.to_string()))?;
        let mut models: Vec<WireModel> = wire
            .models
            .into_iter()
            .filter(|model| matches!(model.visibility.as_deref(), None | Some("list")))
            .collect();
        if models.is_empty() {
            return Err(CatalogError::Empty);
        }
        if models.len() > MAX_CATALOG_MODELS {
            return Err(CatalogError::Malformed(format!(
                "catalog contains more than {MAX_CATALOG_MODELS} listed models"
            )));
        }
        models.sort_by_key(|model| model.priority);

        let source = ModelRuntimeSource::Remote {
            endpoint: MODELS_ENDPOINT.into(),
            fetched_at: fetched_at.into(),
        };
        let mut ordered_slugs = Vec::with_capacity(models.len());
        let mut entries = HashMap::with_capacity(models.len());
        let mut diagnostics = Vec::new();
        for model in models {
            let slug = model.slug.clone();
            if slug.trim().is_empty() || slug.len() > 256 || slug.chars().any(char::is_control) {
                return Err(CatalogError::Malformed(
                    "model slug must contain 1 to 256 bytes and no control characters".into(),
                ));
            }
            if entries.contains_key(&slug) {
                return Err(CatalogError::Malformed(format!(
                    "duplicate model slug {}",
                    bounded_wire_text(&slug, 128)
                )));
            }
            ordered_slugs.push(slug.clone());
            let entry = match descriptor_from_wire(model, source.clone()) {
                Ok(entry) => entry,
                Err(reason) => {
                    diagnostics.push(format!("{slug}: {reason}"));
                    RemoteEntry::Partial {
                        display_name: slug.clone(),
                        reason,
                        source: source.clone(),
                    }
                }
            };
            entries.insert(slug, entry);
        }
        self.remote = Some(RemoteCatalog {
            ordered_slugs,
            entries,
        });
        self.diagnostics = diagnostics;
        Ok(self.models())
    }

    pub fn models(&self) -> Vec<CatalogModel> {
        let slugs = self
            .remote
            .as_ref()
            .map(|remote| remote.ordered_slugs.as_slice())
            .unwrap_or(self.embedded_order.as_slice());
        slugs
            .iter()
            .filter_map(|slug| self.catalog_model(slug))
            .collect()
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn context_window(&self, slug: &str) -> Option<u32> {
        self.descriptor(slug)
            .ok()
            .map(|(descriptor, _)| descriptor.context_window)
    }

    pub fn resolve(
        &self,
        slug: &str,
        reasoning_effort: Option<&str>,
        max_output_tokens: u32,
        retry: ModelRetryPolicy,
    ) -> Result<ResolvedModelRuntime, ModelRuntimeError> {
        let slug = slug.trim();
        let (descriptor, source) = self.descriptor(slug)?;
        descriptor.validate()?;
        if descriptor.tool_mode == ModelToolMode::CodeModeOnly {
            return Err(ModelRuntimeError::Incompatible {
                slug: slug.into(),
                reason: "code mode is required but Pyxis does not implement code mode".into(),
            });
        }
        let effort = reasoning_effort
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string)
            .or_else(|| descriptor.default_reasoning_effort.clone());
        if let Some(effort) = effort.as_deref()
            && !descriptor
                .supported_reasoning_efforts
                .iter()
                .any(|supported| supported.eq_ignore_ascii_case(effort))
        {
            return Err(ModelRuntimeError::UnsupportedReasoningEffort {
                slug: slug.into(),
                effort: effort.into(),
            });
        }

        let mut runtime = ResolvedModelRuntime {
            slug: descriptor.slug.clone(),
            source,
            instructions: descriptor.instructions.clone(),
            fingerprint: String::new(),
            context_window: descriptor.context_window,
            auto_compact_token_limit: descriptor.auto_compact_token_limit,
            input_modalities: descriptor.input_modalities.clone(),
            reasoning_effort: effort,
            supports_verbosity: descriptor.supports_verbosity,
            verbosity: descriptor.default_verbosity.clone(),
            supports_parallel_tool_calls: descriptor.supports_parallel_tool_calls,
            reasoning_replay: descriptor.reasoning_replay,
            responses_dialect: descriptor.responses_dialect,
            tool_mode: descriptor.tool_mode,
            truncation: descriptor.truncation,
            retry,
            max_output_tokens,
            comp_hash: descriptor.comp_hash.clone(),
        };
        runtime.fingerprint = runtime_fingerprint(&runtime)?;
        runtime.validate()?;
        Ok(runtime)
    }

    fn descriptor(
        &self,
        slug: &str,
    ) -> Result<(ModelDescriptor, ModelRuntimeSource), ModelRuntimeError> {
        if let Some(remote) = &self.remote
            && let Some(entry) = remote.entries.get(slug)
        {
            return match entry {
                RemoteEntry::Complete { descriptor, source } => {
                    Ok((descriptor.clone(), source.clone()))
                }
                RemoteEntry::Incompatible { reason, .. } => Err(ModelRuntimeError::Incompatible {
                    slug: slug.into(),
                    reason: reason.clone(),
                }),
                RemoteEntry::Partial { reason, .. } => self
                    .embedded
                    .get(slug)
                    .cloned()
                    .map(|descriptor| {
                        (
                            descriptor,
                            ModelRuntimeSource::Embedded {
                                version: EMBEDDED_CATALOG_VERSION.into(),
                            },
                        )
                    })
                    .ok_or_else(|| ModelRuntimeError::Incompatible {
                        slug: slug.into(),
                        reason: format!("remote descriptor is partial: {reason}"),
                    }),
            };
        }
        self.embedded
            .get(slug)
            .cloned()
            .map(|descriptor| {
                (
                    descriptor,
                    ModelRuntimeSource::Embedded {
                        version: EMBEDDED_CATALOG_VERSION.into(),
                    },
                )
            })
            .ok_or_else(|| ModelRuntimeError::Incompatible {
                slug: slug.into(),
                reason: "no complete remote or embedded descriptor".into(),
            })
    }

    fn catalog_model(&self, slug: &str) -> Option<CatalogModel> {
        let embedded = self.embedded.get(slug);
        let remote = self
            .remote
            .as_ref()
            .and_then(|catalog| catalog.entries.get(slug));
        match remote {
            Some(RemoteEntry::Complete { descriptor, source }) => {
                Some(catalog_model_from_descriptor(descriptor, source.clone()))
            }
            Some(RemoteEntry::Partial {
                display_name,
                reason,
                source,
            }) => embedded
                .map(|descriptor| {
                    catalog_model_from_descriptor(
                        descriptor,
                        ModelRuntimeSource::Embedded {
                            version: EMBEDDED_CATALOG_VERSION.into(),
                        },
                    )
                })
                .or_else(|| {
                    Some(CatalogModel {
                        slug: slug.into(),
                        display_name: display_name.clone(),
                        default_reasoning_effort: None,
                        supported_reasoning_efforts: Vec::new(),
                        context_window: None,
                        source: source.clone(),
                        incompatibility_reason: Some(format!(
                            "remote descriptor is partial: {reason}"
                        )),
                    })
                }),
            Some(RemoteEntry::Incompatible {
                display_name,
                reason,
                source,
            }) => Some(CatalogModel {
                slug: slug.into(),
                display_name: display_name.clone(),
                default_reasoning_effort: None,
                supported_reasoning_efforts: Vec::new(),
                context_window: None,
                source: source.clone(),
                incompatibility_reason: Some(reason.clone()),
            }),
            None => embedded.map(|descriptor| {
                catalog_model_from_descriptor(
                    descriptor,
                    ModelRuntimeSource::Embedded {
                        version: EMBEDDED_CATALOG_VERSION.into(),
                    },
                )
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("model catalog is empty")]
    Empty,
    #[error("malformed model catalog: {0}")]
    Malformed(String),
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
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Option<Vec<WireReasoningLevel>>,
    #[serde(default)]
    base_instructions: Option<String>,
    #[serde(default)]
    model_messages: Option<WireModelMessages>,
    #[serde(default)]
    support_verbosity: Option<bool>,
    #[serde(default)]
    default_verbosity: Option<String>,
    #[serde(default)]
    truncation_policy: Option<WireTruncationPolicy>,
    #[serde(default)]
    supports_parallel_tool_calls: Option<bool>,
    #[serde(default)]
    supports_encrypted_reasoning: Option<bool>,
    #[serde(default)]
    supports_reasoning_replay: Option<bool>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    auto_compact_token_limit: Option<u32>,
    #[serde(default)]
    comp_hash: Option<String>,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    use_responses_lite: Option<bool>,
    #[serde(default)]
    tool_mode: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct WireReasoningLevel {
    effort: String,
}

#[derive(Deserialize)]
struct WireModelMessages {
    #[serde(default)]
    instructions_template: Option<String>,
    #[serde(default)]
    instructions_variables: Option<WireInstructionsVariables>,
}

#[derive(Deserialize)]
struct WireInstructionsVariables {
    #[serde(default)]
    personality_default: Option<String>,
}

#[derive(Deserialize)]
struct WireTruncationPolicy {
    mode: String,
    limit: u32,
}

fn descriptor_from_wire(
    model: WireModel,
    source: ModelRuntimeSource,
) -> Result<RemoteEntry, String> {
    let display_name = model
        .display_name
        .clone()
        .unwrap_or_else(|| model.slug.clone());
    if display_name.len() > 512 || display_name.chars().any(char::is_control) {
        return Ok(RemoteEntry::Incompatible {
            display_name: model.slug,
            reason: "display_name exceeds 512 bytes or contains control characters".into(),
            source,
        });
    }
    let tool_mode = match model.tool_mode {
        None | Some(serde_json::Value::Null) => ModelToolMode::Direct,
        Some(serde_json::Value::String(mode)) if mode == "direct" => ModelToolMode::Direct,
        Some(serde_json::Value::String(mode)) if mode == "code_mode_only" => {
            ModelToolMode::CodeModeOnly
        }
        Some(value) => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                reason: format!(
                    "unknown required tool_mode {}",
                    bounded_wire_text(&value.to_string(), 128)
                ),
                source,
            });
        }
    };
    let templated_instructions = model.model_messages.and_then(|messages| {
        let personality = messages
            .instructions_variables
            .and_then(|variables| variables.personality_default)
            .unwrap_or_default();
        messages
            .instructions_template
            .map(|template| template.replace("{{ personality }}", &personality))
    });
    let instructions = templated_instructions
        .filter(|instructions| !instructions.is_empty())
        .or_else(|| {
            model
                .base_instructions
                .filter(|instructions| !instructions.is_empty())
        })
        .ok_or_else(|| "missing base instructions".to_string())?;
    let context_window = model
        .context_window
        .filter(|window| *window > 0)
        .ok_or_else(|| "missing positive context_window".to_string())?;
    let auto_compact_token_limit = model
        .auto_compact_token_limit
        .unwrap_or_else(|| context_window.saturating_mul(9) / 10)
        .min(context_window.saturating_mul(9) / 10);
    let raw_modalities = model
        .input_modalities
        .ok_or_else(|| "missing input_modalities".to_string())?;
    let mut modalities = Vec::with_capacity(raw_modalities.len());
    for modality in raw_modalities {
        match modality.as_str() {
            "text" => modalities.push(InputModality::Text),
            "image" => modalities.push(InputModality::Image),
            other => {
                return Ok(RemoteEntry::Incompatible {
                    display_name,
                    reason: format!(
                        "unknown required input modality {}",
                        bounded_wire_text(other, 128)
                    ),
                    source,
                });
            }
        }
    }
    let reasoning = model
        .supported_reasoning_levels
        .ok_or_else(|| "missing supported_reasoning_levels".to_string())?
        .into_iter()
        .map(|level| level.effort)
        .collect::<Vec<_>>();
    let supports_verbosity = model
        .support_verbosity
        .ok_or_else(|| "missing support_verbosity".to_string())?;
    let parallel = model
        .supports_parallel_tool_calls
        .ok_or_else(|| "missing supports_parallel_tool_calls".to_string())?;
    let lite = model
        .use_responses_lite
        .ok_or_else(|| "missing use_responses_lite".to_string())?;
    let truncation = model
        .truncation_policy
        .ok_or_else(|| "missing truncation_policy".to_string())?;
    let truncation_mode = match truncation.mode.as_str() {
        "bytes" => TruncationMode::Bytes,
        "tokens" => TruncationMode::Tokens,
        other => {
            return Ok(RemoteEntry::Incompatible {
                display_name,
                reason: format!(
                    "unknown required truncation mode {}",
                    bounded_wire_text(other, 128)
                ),
                source,
            });
        }
    };
    let descriptor = ModelDescriptor {
        slug: model.slug,
        display_name,
        instructions,
        context_window,
        auto_compact_token_limit,
        input_modalities: modalities,
        supports_reasoning: !reasoning.is_empty(),
        default_reasoning_effort: model.default_reasoning_level,
        supported_reasoning_efforts: reasoning,
        supports_verbosity,
        default_verbosity: model.default_verbosity,
        supports_parallel_tool_calls: parallel,
        reasoning_replay: if model.supports_encrypted_reasoning == Some(true)
            && model.supports_reasoning_replay == Some(true)
        {
            ReasoningReplaySupport::Enabled
        } else {
            ReasoningReplaySupport::Disabled
        },
        responses_dialect: if lite {
            ResponsesDialect::Lite
        } else {
            ResponsesDialect::Standard
        },
        tool_mode,
        truncation: TruncationPolicy {
            mode: truncation_mode,
            limit: truncation.limit,
        },
        comp_hash: model.comp_hash,
    };
    if let Err(error) = descriptor.validate() {
        return Ok(RemoteEntry::Incompatible {
            display_name: descriptor.display_name.clone(),
            reason: error.to_string(),
            source,
        });
    }
    Ok(RemoteEntry::Complete { descriptor, source })
}

fn bounded_wire_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars
        .by_ref()
        .take(max_chars)
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

fn catalog_model_from_descriptor(
    descriptor: &ModelDescriptor,
    source: ModelRuntimeSource,
) -> CatalogModel {
    CatalogModel {
        slug: descriptor.slug.clone(),
        display_name: descriptor.display_name.clone(),
        default_reasoning_effort: descriptor.default_reasoning_effort.clone(),
        supported_reasoning_efforts: descriptor.supported_reasoning_efforts.clone(),
        context_window: Some(descriptor.context_window),
        source,
        incompatibility_reason: (descriptor.tool_mode == ModelToolMode::CodeModeOnly)
            .then(|| "code mode is required but Pyxis does not implement code mode".to_string()),
    }
}

fn runtime_fingerprint(runtime: &ResolvedModelRuntime) -> Result<String, ModelRuntimeError> {
    let bytes = serde_json::to_vec(runtime).map_err(|error| ModelRuntimeError::InvalidField {
        field: "runtime",
        detail: error.to_string(),
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[allow(clippy::too_many_arguments)]
fn descriptor(
    slug: &str,
    display_name: &str,
    instructions: &str,
    default_reasoning_effort: &str,
    supported_reasoning_efforts: &[&str],
    verbosity: &str,
    dialect: ResponsesDialect,
    tool_mode: ModelToolMode,
    comp_hash: &str,
) -> ModelDescriptor {
    ModelDescriptor {
        slug: slug.into(),
        display_name: display_name.into(),
        instructions: instructions.into(),
        context_window: 272_000,
        auto_compact_token_limit: 244_800,
        input_modalities: vec![InputModality::Text, InputModality::Image],
        supports_reasoning: true,
        default_reasoning_effort: Some(default_reasoning_effort.into()),
        supported_reasoning_efforts: supported_reasoning_efforts
            .iter()
            .map(|effort| (*effort).into())
            .collect(),
        supports_verbosity: true,
        default_verbosity: Some(verbosity.into()),
        supports_parallel_tool_calls: true,
        reasoning_replay: ReasoningReplaySupport::Enabled,
        responses_dialect: dialect,
        tool_mode,
        truncation: TruncationPolicy {
            mode: TruncationMode::Tokens,
            limit: 10_000,
        },
        comp_hash: Some(comp_hash.into()),
    }
}

fn embedded_descriptors() -> Vec<ModelDescriptor> {
    const STANDARD_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
    const FRONTIER_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
    vec![
        descriptor(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            GENERIC_INSTRUCTIONS,
            "low",
            FRONTIER_EFFORTS,
            "low",
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            "3000",
        ),
        descriptor(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            GENERIC_INSTRUCTIONS,
            "medium",
            FRONTIER_EFFORTS,
            "low",
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            "3000",
        ),
        descriptor(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            GENERIC_INSTRUCTIONS,
            "medium",
            &["low", "medium", "high", "xhigh", "max"],
            "low",
            ResponsesDialect::Lite,
            ModelToolMode::CodeModeOnly,
            "3000",
        ),
        descriptor(
            "gpt-5.5",
            "GPT-5.5",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "low",
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            "2911",
        ),
        descriptor(
            "gpt-5.4",
            "GPT-5.4",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "low",
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            "2911",
        ),
        descriptor(
            "gpt-5.4-mini",
            "GPT-5.4 Mini",
            GENERIC_INSTRUCTIONS,
            "medium",
            STANDARD_EFFORTS,
            "medium",
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            "2911",
        ),
        descriptor(
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            CODEX_INSTRUCTIONS,
            "high",
            STANDARD_EFFORTS,
            "low",
            ResponsesDialect::Standard,
            ModelToolMode::Direct,
            "2026-07-24",
        ),
    ]
}

/// Compatibility helper used by callers that only need an effective listing.
pub fn parse_catalog(body: &str) -> Result<Vec<CatalogModel>, CatalogError> {
    let mut catalog = ModelCatalog::embedded();
    catalog.install_remote(body, "unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RICH_FIXTURE: &str = include_str!("../fixtures/models-2026-07-28.json");

    #[test]
    fn rich_fixture_preserves_every_runtime_field() {
        let mut catalog = ModelCatalog::embedded();
        let models = catalog
            .install_remote(RICH_FIXTURE, "2026-07-28")
            .expect("fixture parses");
        assert_eq!(
            models
                .iter()
                .map(|model| model.slug.as_str())
                .collect::<Vec<_>>(),
            ["fixture-lite", "gpt-5.5"]
        );
        let runtime = catalog
            .resolve(
                "fixture-lite",
                Some("high"),
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect("runtime resolves");
        assert_eq!(runtime.instructions, "fixture base instructions");
        assert_eq!(runtime.context_window, 200_000);
        assert_eq!(runtime.auto_compact_token_limit, 170_000);
        assert_eq!(runtime.responses_dialect, ResponsesDialect::Lite);
        assert_eq!(runtime.tool_mode, ModelToolMode::Direct);
        assert_eq!(runtime.truncation.limit, 9_000);
        assert_eq!(runtime.comp_hash.as_deref(), Some("fixture-1"));
        assert!(runtime.supports_parallel_tool_calls);
        assert!(runtime.accepts_images());
        assert_eq!(runtime.reasoning_replay, ReasoningReplaySupport::Disabled);
        assert!(runtime.reasoning_replay_disabled_reason().is_some());
    }

    #[test]
    fn remote_replay_requires_both_encrypted_and_stateless_proofs() {
        let body = RICH_FIXTURE.replace(
            r#""supports_parallel_tool_calls": true,"#,
            r#""supports_parallel_tool_calls": true,
      "supports_encrypted_reasoning": true,
      "supports_reasoning_replay": true,"#,
        );
        let mut catalog = ModelCatalog::embedded();
        catalog.install_remote(&body, "2026-07-28").unwrap();
        let runtime = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .unwrap();
        assert_eq!(runtime.reasoning_replay, ReasoningReplaySupport::Enabled);
        assert!(runtime.reasoning_replay_disabled_reason().is_none());
    }

    #[test]
    fn remote_complete_wins_and_partial_uses_whole_embedded_descriptor() {
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(RICH_FIXTURE, "2026-07-28")
            .expect("fixture parses");
        let runtime = catalog
            .resolve(
                "gpt-5.5",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect("embedded fallback resolves");
        assert!(matches!(
            runtime.source,
            ModelRuntimeSource::Embedded { .. }
        ));
        assert_eq!(runtime.context_window, 272_000);
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.contains("missing base instructions"))
        );
    }

    #[test]
    fn null_auto_compact_limit_uses_the_reference_ninety_percent_rule() {
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(
                &RICH_FIXTURE.replace(
                    r#""auto_compact_token_limit": 170000"#,
                    r#""auto_compact_token_limit": null"#,
                ),
                "2026-07-28",
            )
            .expect("current catalog null remains a complete descriptor");
        let runtime = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect("runtime resolves");
        assert_eq!(runtime.auto_compact_token_limit, 180_000);
    }

    #[test]
    fn instruction_template_and_default_personality_override_base_instructions() {
        let body = RICH_FIXTURE.replace(
            r#""base_instructions": "fixture base instructions","#,
            r#""base_instructions": "fixture base instructions",
      "model_messages": {
        "instructions_template": "before {{ personality }} after",
        "instructions_variables": {
          "personality_default": "default voice"
        }
      },"#,
        );
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(&body, "2026-07-28")
            .expect("templated descriptor parses");
        let runtime = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect("runtime resolves");
        assert_eq!(runtime.instructions, "before default voice after");
    }

    #[test]
    fn malformed_or_empty_refresh_preserves_last_valid_catalog() {
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(RICH_FIXTURE, "2026-07-28")
            .expect("fixture parses");
        let before = catalog.models();
        assert!(catalog.install_remote("{", "later").is_err());
        assert_eq!(catalog.models(), before);
        assert!(catalog.install_remote(r#"{"models":[]}"#, "later").is_err());
        assert_eq!(catalog.models(), before);
    }

    #[test]
    fn unknown_tool_mode_is_explicitly_incompatible() {
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(
                &RICH_FIXTURE.replace("\"direct\"", "\"future_mode\""),
                "2026-07-28",
            )
            .expect("catalog shape remains valid");
        let error = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect_err("unknown required mode must fail closed");
        assert!(error.to_string().contains("future_mode"));
    }

    #[test]
    fn terminal_control_characters_in_catalog_identifiers_are_rejected() {
        let mut catalog = ModelCatalog::embedded();
        let error = catalog
            .install_remote(
                &RICH_FIXTURE.replace("fixture-lite", r"fixture-\u001b]52;evil"),
                "2026-07-28",
            )
            .expect_err("control characters must invalidate the snapshot");
        assert!(error.to_string().contains("control characters"));
    }

    #[test]
    fn unknown_mandatory_capability_is_not_replaced_by_embedded_defaults() {
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(
                &RICH_FIXTURE.replace(
                    r#""input_modalities": ["text", "image"]"#,
                    r#""input_modalities": ["text", "future_modality"]"#,
                ),
                "2026-07-28",
            )
            .expect("catalog shape remains valid");
        let error = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect_err("unknown mandatory capability must fail closed");
        assert!(error.to_string().contains("future_modality"));
    }

    #[test]
    fn oversized_remote_instructions_are_incompatible_before_sampling() {
        let oversized = "x".repeat(agent_core::model::MAX_MODEL_INSTRUCTIONS_BYTES + 1);
        let mut catalog = ModelCatalog::embedded();
        catalog
            .install_remote(
                &RICH_FIXTURE.replace("fixture base instructions", &oversized),
                "2026-07-28",
            )
            .expect("catalog shape remains valid");
        let error = catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect_err("oversized instructions must fail closed");
        assert!(error.to_string().contains("instructions exceed"));
    }

    #[test]
    fn code_mode_only_models_are_listed_but_cannot_start() {
        let catalog = ModelCatalog::embedded();
        let listed = catalog
            .models()
            .into_iter()
            .find(|model| model.slug == "gpt-5.6-sol")
            .expect("embedded frontier model");
        assert!(
            listed
                .incompatibility_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("code mode"))
        );
        let error = catalog
            .resolve(
                "gpt-5.6-sol",
                None,
                4096,
                ModelRetryPolicy {
                    max_retries: 3,
                    backoff_base_ms: 50,
                },
            )
            .expect_err("code mode is not implemented");
        assert!(error.to_string().contains("code mode"));
    }

    #[test]
    fn fingerprints_match_for_identical_interactive_and_headless_sources() {
        let catalog = ModelCatalog::embedded();
        let resolve = || {
            catalog
                .resolve(
                    "gpt-5.5",
                    Some("high"),
                    4096,
                    ModelRetryPolicy {
                        max_retries: 3,
                        backoff_base_ms: 50,
                    },
                )
                .expect("runtime resolves")
        };
        assert_eq!(resolve().fingerprint, resolve().fingerprint);
    }
}
