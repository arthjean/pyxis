//! Rich model catalog and deterministic runtime resolution.
//!
//! The remote `/models` payload wins only as a complete descriptor. A malformed
//! refresh leaves the last valid catalog installed; a partial entry falls back
//! as a whole to the versioned embedded entry. Explicit unknown required
//! capabilities stay incompatible rather than degrading to direct tools.

mod embedded;
mod wire;

use std::collections::HashMap;

use agent_core::model::{
    InputModality, ModelDescriptor, ModelRetryPolicy, ModelRuntimeError, ModelRuntimeSource,
    ModelToolCapabilities, ModelToolMode, MultiAgentVersion, ResolvedModelRuntime,
};
use sha2::{Digest, Sha256};

use embedded::embedded_descriptors;
use wire::{WireCatalog, bounded_wire_text, catalog_metadata_from_wire, descriptor_from_wire};

pub const EMBEDDED_CATALOG_VERSION: &str = "2026-07-28.codex-8e271dc02b23";
pub const MODELS_ENDPOINT: &str = "/backend-api/codex/models";
const MAX_CATALOG_MODELS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub context_window: Option<u32>,
    pub metadata: CatalogMetadata,
    pub source: ModelRuntimeSource,
    pub incompatibility_reason: Option<String>,
}

/// Provider catalog fields which affect discovery or capability selection but
/// are not part of the sampling runtime yet. Keeping them typed here prevents
/// `/models` from becoming a lossy slug-only endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogMetadata {
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub supported_in_api: Option<bool>,
    pub priority: i32,
    pub reasoning_level_descriptions: Vec<(String, String)>,
    pub additional_speed_tiers: Vec<String>,
    pub service_tiers: Vec<CatalogServiceTier>,
    pub default_service_tier: Option<String>,
    pub availability_nux: Option<serde_json::Value>,
    pub upgrade: Option<serde_json::Value>,
    pub include_skills_usage_instructions: bool,
    pub supports_reasoning_summary_parameter: bool,
    pub default_reasoning_summary: Option<serde_json::Value>,
    pub shell_type: Option<String>,
    pub apply_patch_tool_type: Option<serde_json::Value>,
    pub web_search_tool_type: Option<serde_json::Value>,
    pub supports_image_detail_original: bool,
    pub max_context_window: Option<u32>,
    pub effective_context_window_percent: u8,
    pub experimental_supported_tools: Vec<String>,
    pub input_modalities: Vec<String>,
    pub supports_search_tool: bool,
    pub auto_review_model_override: Option<String>,
    /// Unknown top-level field names are retained as diagnostics. Values are
    /// deliberately not retained because future catalog fields may be secret.
    pub unrecognized_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogServiceTier {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatalogScope {
    pub provider: String,
    pub endpoint: String,
    /// SHA-256 of the provider's non-secret account identifier.
    pub identity_fingerprint: String,
}

#[derive(Debug, Clone)]
enum RemoteEntry {
    Complete {
        descriptor: Box<ModelDescriptor>,
        metadata: Box<CatalogMetadata>,
        source: ModelRuntimeSource,
    },
    Partial {
        display_name: String,
        metadata: Box<CatalogMetadata>,
        reason: String,
        source: ModelRuntimeSource,
    },
    Incompatible {
        display_name: String,
        metadata: Box<CatalogMetadata>,
        reason: String,
        source: ModelRuntimeSource,
    },
}

#[derive(Debug, Clone)]
struct RemoteCatalog {
    scope: CatalogScope,
    etag: Option<String>,
    ordered_slugs: Vec<String>,
    entries: HashMap<String, RemoteEntry>,
}

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    local: HashMap<String, ModelDescriptor>,
    local_order: Vec<String>,
    local_source: Option<ModelRuntimeSource>,
    remote: Option<RemoteCatalog>,
    diagnostics: Vec<String>,
    /// Is a Code Mode runtime wired in this process? Fail-closed default
    /// `false`: without one, a model that needs code mode is refused BEFORE any
    /// provider call, naming the missing component instead of failing later
    /// with an empty answer.
    code_mode: bool,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::embedded()
    }
}

impl ModelCatalog {
    pub fn embedded() -> Self {
        let descriptors = embedded_descriptors();
        let local_order = descriptors
            .iter()
            .map(|descriptor| descriptor.slug.clone())
            .collect();
        let local = descriptors
            .into_iter()
            .map(|descriptor| (descriptor.slug.clone(), descriptor))
            .collect();
        Self {
            local,
            local_order,
            local_source: Some(ModelRuntimeSource::Embedded {
                version: EMBEDDED_CATALOG_VERSION.into(),
            }),
            remote: None,
            diagnostics: Vec::new(),
            code_mode: false,
        }
    }

    /// Authoritative in-memory catalog used when provider configuration names
    /// every model. Callers never attach a remote fetch path to this value.
    pub fn from_static(descriptors: Vec<ModelDescriptor>) -> Result<Self, CatalogError> {
        if descriptors.is_empty() {
            return Err(CatalogError::Empty);
        }
        let mut local = HashMap::with_capacity(descriptors.len());
        let mut local_order = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            descriptor
                .validate()
                .map_err(|error| CatalogError::Malformed(error.to_string()))?;
            if local.contains_key(&descriptor.slug) {
                return Err(CatalogError::Malformed(format!(
                    "duplicate model slug {}",
                    bounded_wire_text(&descriptor.slug, 128)
                )));
            }
            local_order.push(descriptor.slug.clone());
            local.insert(descriptor.slug.clone(), descriptor);
        }
        Ok(Self {
            local,
            local_order,
            local_source: Some(ModelRuntimeSource::Configured),
            remote: None,
            diagnostics: Vec::new(),
            code_mode: false,
        })
    }

    /// Empty catalog for providers whose only source of truth is a remote,
    /// scope-bound snapshot. It never falls back to ChatGPT's embedded models.
    pub fn remote_only() -> Self {
        Self {
            local: HashMap::new(),
            local_order: Vec::new(),
            local_source: None,
            remote: None,
            diagnostics: Vec::new(),
            code_mode: false,
        }
    }

    /// Declares whether this process can run Code Mode cells. Set once, by the
    /// binary that owns the runtime; the catalog itself knows nothing about V8.
    pub fn set_code_mode(&mut self, available: bool) {
        self.code_mode = available;
    }

    pub fn code_mode(&self) -> bool {
        self.code_mode
    }

    /// Installs one complete remote snapshot. Errors and empty snapshots leave
    /// the previous valid state untouched.
    pub fn install_remote(
        &mut self,
        body: &str,
        fetched_at: &str,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        self.install_remote_scoped(
            body,
            fetched_at,
            CatalogScope {
                provider: "openai_chatgpt".into(),
                endpoint: MODELS_ENDPOINT.into(),
                identity_fingerprint: "legacy-local-scope".into(),
            },
            None,
        )
    }

    /// Installs a snapshot under an explicit provider, endpoint and identity
    /// key. Changing scope first evicts the old remote data, including when the
    /// new response is malformed, so stale retention can never cross accounts.
    pub fn install_remote_scoped(
        &mut self,
        body: &str,
        fetched_at: &str,
        scope: CatalogScope,
        etag: Option<String>,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        self.ensure_scope(scope.clone());
        let wire: WireCatalog = serde_json::from_str(body)
            .map_err(|error| CatalogError::Malformed(error.to_string()))?;
        let mut models = wire.models;
        if models.is_empty() {
            return Err(CatalogError::Empty);
        }
        if models.len() > MAX_CATALOG_MODELS {
            return Err(CatalogError::Malformed(format!(
                "catalog contains more than {MAX_CATALOG_MODELS} models"
            )));
        }
        models.sort_by_key(|model| model.priority);

        let source = ModelRuntimeSource::Remote {
            endpoint: scope.endpoint.clone(),
            fetched_at: fetched_at.into(),
        };
        let mut ordered_slugs = Vec::with_capacity(models.len());
        let mut entries = HashMap::with_capacity(models.len());
        let mut diagnostics = Vec::new();
        for model in models {
            let slug = model.slug.clone();
            let listed = matches!(model.visibility.as_deref(), None | Some("list"));
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
            if listed {
                ordered_slugs.push(slug.clone());
            }
            let metadata = catalog_metadata_from_wire(&model);
            for field in &metadata.unrecognized_fields {
                diagnostics.push(format!("{slug}: unrecognized catalog field {field}"));
            }
            let entry =
                match descriptor_from_wire(model, Box::new(metadata.clone()), source.clone()) {
                    Ok(entry) => entry,
                    Err(reason) => {
                        diagnostics.push(format!("{slug}: {reason}"));
                        RemoteEntry::Partial {
                            display_name: slug.clone(),
                            metadata: Box::new(metadata),
                            reason,
                            source: source.clone(),
                        }
                    }
                };
            if let RemoteEntry::Incompatible { reason, .. } = &entry {
                diagnostics.push(format!("{slug}: {reason}"));
            }
            entries.insert(slug, entry);
        }
        self.remote = Some(RemoteCatalog {
            scope,
            etag: etag.filter(|value| !value.trim().is_empty()),
            ordered_slugs,
            entries,
        });
        self.diagnostics = diagnostics;
        Ok(self.models())
    }

    pub fn ensure_scope(&mut self, scope: CatalogScope) {
        if self
            .remote
            .as_ref()
            .is_some_and(|remote| remote.scope != scope)
        {
            self.remote = None;
            self.diagnostics.clear();
        }
    }

    pub fn clear_remote(&mut self) {
        self.remote = None;
        self.diagnostics.clear();
    }

    pub fn scope(&self) -> Option<&CatalogScope> {
        self.remote.as_ref().map(|remote| &remote.scope)
    }

    pub fn etag(&self) -> Option<&str> {
        self.remote
            .as_ref()
            .and_then(|remote| remote.etag.as_deref())
    }

    pub fn models(&self) -> Vec<CatalogModel> {
        let slugs = self
            .remote
            .as_ref()
            .map(|remote| remote.ordered_slugs.as_slice())
            .unwrap_or(self.local_order.as_slice());
        slugs
            .iter()
            .filter_map(|slug| self.catalog_model(slug))
            .collect()
    }

    /// Looks up any decoded entry, including a hidden model omitted from picker
    /// ordering. Visibility is a presentation property, not data deletion.
    pub fn model(&self, slug: &str) -> Option<CatalogModel> {
        self.catalog_model(slug)
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Tool mode of a slug, without resolving a runtime. Used at a step
    /// boundary, where composing the tool plan must not cost a full resolve.
    pub fn tool_mode(&self, slug: &str) -> Option<ModelToolMode> {
        self.descriptor(slug)
            .ok()
            .map(|(descriptor, _)| descriptor.tool_mode)
    }

    /// Tool contract of a slug, without resolving a runtime. Read at the same
    /// step boundary as [`Self::tool_mode`]: composing a hosted tool has to know
    /// whether the model was declared able to use it.
    pub fn tool_capabilities(&self, slug: &str) -> Option<ModelToolCapabilities> {
        self.descriptor(slug)
            .ok()
            .map(|(descriptor, _)| descriptor.tool_capabilities.clone())
    }

    /// Orchestration protocol of a slug, without resolving a runtime. Read at
    /// the same step boundary as [`Self::tool_mode`], for the same reason.
    pub fn multi_agent_version(&self, slug: &str) -> Option<MultiAgentVersion> {
        self.descriptor(slug)
            .ok()
            .map(|(descriptor, _)| descriptor.multi_agent_version)
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
        if let Some(reason) = code_mode_incompatibility(descriptor.tool_mode, self.code_mode) {
            return Err(ModelRuntimeError::Incompatible {
                slug: slug.into(),
                reason,
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
            tool_capabilities: descriptor.tool_capabilities.clone(),
            service_tiers: descriptor.service_tiers.clone(),
            reasoning_replay: descriptor.reasoning_replay,
            responses_dialect: descriptor.responses_dialect,
            tool_mode: descriptor.tool_mode,
            multi_agent_version: descriptor.multi_agent_version,
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
                RemoteEntry::Complete {
                    descriptor, source, ..
                } => Ok((descriptor.as_ref().clone(), source.clone())),
                RemoteEntry::Incompatible { reason, .. } => Err(ModelRuntimeError::Incompatible {
                    slug: slug.into(),
                    reason: reason.clone(),
                }),
                RemoteEntry::Partial { reason, .. } => {
                    self.local_descriptor(slug)
                        .ok_or_else(|| ModelRuntimeError::Incompatible {
                            slug: slug.into(),
                            reason: format!("remote descriptor is partial: {reason}"),
                        })
                }
            };
        }
        self.local_descriptor(slug)
            .ok_or_else(|| ModelRuntimeError::Incompatible {
                slug: slug.into(),
                reason: "no complete remote or local descriptor".into(),
            })
    }

    fn local_descriptor(&self, slug: &str) -> Option<(ModelDescriptor, ModelRuntimeSource)> {
        Some((
            self.local.get(slug)?.clone(),
            self.local_source.as_ref()?.clone(),
        ))
    }

    fn catalog_model(&self, slug: &str) -> Option<CatalogModel> {
        let local = self.local.get(slug);
        let remote = self
            .remote
            .as_ref()
            .and_then(|catalog| catalog.entries.get(slug));
        match remote {
            Some(RemoteEntry::Complete {
                descriptor,
                metadata,
                source,
            }) => Some(catalog_model_from_descriptor(
                descriptor,
                metadata.as_ref().clone(),
                source.clone(),
                self.code_mode,
            )),
            Some(RemoteEntry::Partial {
                display_name,
                metadata,
                reason,
                source,
            }) => local
                .zip(self.local_source.as_ref())
                .map(|(descriptor, source)| {
                    catalog_model_from_descriptor(
                        descriptor,
                        metadata.as_ref().clone(),
                        source.clone(),
                        self.code_mode,
                    )
                })
                .or_else(|| {
                    Some(CatalogModel {
                        slug: slug.into(),
                        display_name: display_name.clone(),
                        default_reasoning_effort: None,
                        supported_reasoning_efforts: Vec::new(),
                        context_window: None,
                        metadata: metadata.as_ref().clone(),
                        source: source.clone(),
                        incompatibility_reason: Some(format!(
                            "remote descriptor is partial: {reason}"
                        )),
                    })
                }),
            Some(RemoteEntry::Incompatible {
                display_name,
                metadata,
                reason,
                source,
            }) => Some(CatalogModel {
                slug: slug.into(),
                display_name: display_name.clone(),
                default_reasoning_effort: None,
                supported_reasoning_efforts: Vec::new(),
                context_window: None,
                metadata: metadata.as_ref().clone(),
                source: source.clone(),
                incompatibility_reason: Some(reason.clone()),
            }),
            None => local
                .zip(self.local_source.as_ref())
                .map(|(descriptor, source)| {
                    catalog_model_from_descriptor(
                        descriptor,
                        local_catalog_metadata(descriptor),
                        source.clone(),
                        self.code_mode,
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

/// The one place that decides whether a tool mode is usable here. `None` means
/// usable; `Some(reason)` names the missing component, which is what edge case
/// 1 of the PRD requires the user to see instead of a silent non-answer.
fn code_mode_incompatibility(mode: ModelToolMode, code_mode_available: bool) -> Option<String> {
    (mode.needs_code_mode() && !code_mode_available).then(|| {
        "this model requires Code Mode, but no Code Mode runtime is available in this build"
            .to_string()
    })
}

fn catalog_model_from_descriptor(
    descriptor: &ModelDescriptor,
    metadata: CatalogMetadata,
    source: ModelRuntimeSource,
    code_mode_available: bool,
) -> CatalogModel {
    CatalogModel {
        slug: descriptor.slug.clone(),
        display_name: descriptor.display_name.clone(),
        default_reasoning_effort: descriptor.default_reasoning_effort.clone(),
        supported_reasoning_efforts: descriptor.supported_reasoning_efforts.clone(),
        context_window: Some(descriptor.context_window),
        metadata,
        source,
        incompatibility_reason: code_mode_incompatibility(
            descriptor.tool_mode,
            code_mode_available,
        ),
    }
}

fn local_catalog_metadata(descriptor: &ModelDescriptor) -> CatalogMetadata {
    CatalogMetadata {
        visibility: Some("list".into()),
        supported_in_api: Some(true),
        service_tiers: descriptor
            .service_tiers
            .iter()
            .map(|id| CatalogServiceTier {
                id: id.clone(),
                name: None,
                description: None,
            })
            .collect(),
        default_service_tier: descriptor.service_tiers.first().cloned(),
        supports_reasoning_summary_parameter: true,
        effective_context_window_percent: 95,
        input_modalities: descriptor
            .input_modalities
            .iter()
            .map(|modality| match modality {
                InputModality::Text => "text".to_string(),
                InputModality::Image => "image".to_string(),
                InputModality::Audio => "audio".to_string(),
            })
            .collect(),
        ..CatalogMetadata::default()
    }
}

fn runtime_fingerprint(runtime: &ResolvedModelRuntime) -> Result<String, ModelRuntimeError> {
    let bytes = serde_json::to_vec(runtime).map_err(|error| ModelRuntimeError::InvalidField {
        field: "runtime",
        detail: error.to_string(),
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Compatibility helper used by callers that only need an effective listing.
pub fn parse_catalog(body: &str) -> Result<Vec<CatalogModel>, CatalogError> {
    let mut catalog = ModelCatalog::embedded();
    catalog.install_remote(body, "unknown")
}

#[cfg(test)]
mod tests;
