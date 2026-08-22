//! Codex parity baseline: one deterministic contract matrix derived from a
//! read-only Codex clone pinned at a single commit (EP-001/US-001).
//!
//! Why a generated matrix instead of a written audit: a prose audit is a
//! snapshot that ages silently as soon as Codex moves. The matrix is extracted
//! from the baseline sources, fingerprinted, committed, and re-checkable, so a
//! divergence is a failing command instead of a stale paragraph. The historical
//! audits in `docs/` remain context; this file is the normative target.
//!
//! Invariants:
//! - the clone is NEVER written to (only `git rev-parse` and file reads);
//! - a missing clone, a non-repository, or another commit fails with the
//!   expected path AND the expected commit, before any extraction;
//! - extraction fails loudly when a baseline source stops matching the shape it
//!   is parsed with: an unparsable source is upstream drift, not an empty
//!   section.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod client_model;

/// Normative Codex commit for this parity campaign (PRD hard constraint).
pub const BASELINE_COMMIT: &str = "343074d4207d572809bd8cea15f4be1d09d98e0b";

/// Default location of the read-only clone. Overridable so the verifier is not
/// tied to one machine.
pub const DEFAULT_BASELINE_PATH: &str = "/home/arthur/dev/codex";

pub const BASELINE_PATH_ENV: &str = "PYXIS_CODEX_BASELINE";

/// Bumped whenever the matrix shape changes, so an old committed matrix is
/// rejected instead of silently compared field by field.
pub const MATRIX_SCHEMA_VERSION: u32 = 1;

/// Committed matrix, relative to the Pyxis repository root.
pub const COMMITTED_MATRIX_PATH: &str = "docs/parity/codex-baseline-matrix.json";

const MODELS_SOURCE: &str = "codex-rs/models-manager/models.json";
const TOOL_MODE_SOURCE: &str = "codex-rs/protocol/src/openai_models.rs";
const MULTI_AGENT_VERSION_SOURCE: &str = "codex-rs/protocol/src/protocol.rs";
const MULTI_AGENT_V1_NAMESPACE_SOURCE: &str =
    "codex-rs/core/src/tools/handlers/multi_agents_spec.rs";
const MULTI_AGENT_V2_NAMESPACE_SOURCE: &str = "codex-rs/core/src/config/mod.rs";
const MULTI_AGENT_V1_HANDLERS: &str = "codex-rs/core/src/tools/handlers/multi_agents";
const MULTI_AGENT_V2_HANDLERS: &str = "codex-rs/core/src/tools/handlers/multi_agents_v2";
const APP_SERVER_SOURCE: &str = "codex-rs/app-server-protocol/src/protocol/common.rs";

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error(
        "codex baseline clone not found at {path} (expected commit {expected_commit}); set {BASELINE_PATH_ENV} to the read-only clone"
    )]
    MissingClone {
        path: PathBuf,
        expected_commit: String,
    },
    #[error(
        "{path} is not a readable git repository (expected commit {expected_commit}): {detail}"
    )]
    NotAGitRepository {
        path: PathBuf,
        expected_commit: String,
        detail: String,
    },
    #[error("{path} is at commit {actual_commit}, expected baseline commit {expected_commit}")]
    CommitMismatch {
        path: PathBuf,
        expected_commit: String,
        actual_commit: String,
    },
    #[error(
        "codex baseline at {path} has tracked changes (expected clean commit {expected_commit}): {files}"
    )]
    DirtyTracked {
        path: PathBuf,
        expected_commit: String,
        files: String,
    },
    #[error("baseline source {file} is unreadable under {path} (commit {commit}): {detail}")]
    UnreadableSource {
        file: String,
        path: PathBuf,
        commit: String,
        detail: String,
    },
    /// The source exists but no longer matches the shape it is parsed with.
    /// Upstream drift, reported with the source and what was expected.
    #[error("baseline source {file} no longer matches the expected shape: {detail}")]
    SourceDrift { file: String, detail: String },
}

/// A verified handle on the read-only clone. Construction is the verification:
/// no extraction can run against an unpinned clone.
#[derive(Debug, Clone)]
pub struct CodexBaseline {
    root: PathBuf,
    commit: String,
}

impl CodexBaseline {
    /// Resolves the clone from `PYXIS_CODEX_BASELINE`, else the default path.
    pub fn from_env() -> Result<Self, BaselineError> {
        let path = std::env::var_os(BASELINE_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BASELINE_PATH));
        Self::open(path)
    }

    /// Opens the clone and verifies its HEAD against [`BASELINE_COMMIT`].
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BaselineError> {
        Self::open_at_commit(root, BASELINE_COMMIT)
    }

    /// Opens the clone at WHATEVER commit it is on, for the drift report
    /// (EP-006/US-020 AC4).
    ///
    /// This is the one entry point that does not pin, and it exists so a
    /// maintainer can read what moved upstream WITHOUT moving the baseline: the
    /// matrix it extracts is printed and diffed, never written. Pinning stays
    /// the rule for `check` and `generate`, which is what keeps a campaign
    /// anchored to one commit.
    pub fn open_unpinned(root: impl AsRef<Path>) -> Result<Self, BaselineError> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(BaselineError::MissingClone {
                path: root,
                expected_commit: BASELINE_COMMIT.to_string(),
            });
        }
        let commit = head_of(&root, BASELINE_COMMIT)?;
        Ok(Self { root, commit })
    }

    pub fn open_at_commit(
        root: impl AsRef<Path>,
        expected_commit: &str,
    ) -> Result<Self, BaselineError> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(BaselineError::MissingClone {
                path: root,
                expected_commit: expected_commit.to_string(),
            });
        }
        let actual_commit = head_of(&root, expected_commit)?;
        if actual_commit != expected_commit {
            return Err(BaselineError::CommitMismatch {
                path: root,
                expected_commit: expected_commit.to_string(),
                actual_commit,
            });
        }
        ensure_tracked_clean(&root, expected_commit)?;
        Ok(Self {
            root,
            commit: actual_commit,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn commit(&self) -> &str {
        &self.commit
    }

    fn read(&self, source: &str) -> Result<String, BaselineError> {
        std::fs::read_to_string(self.root.join(source)).map_err(|error| {
            BaselineError::UnreadableSource {
                file: source.to_string(),
                path: self.root.clone(),
                commit: self.commit.clone(),
                detail: error.to_string(),
            }
        })
    }

    fn read_dir_sources(&self, directory: &str) -> Result<Vec<(String, String)>, BaselineError> {
        let path = self.root.join(directory);
        let entries =
            std::fs::read_dir(&path).map_err(|error| BaselineError::UnreadableSource {
                file: directory.to_string(),
                path: self.root.clone(),
                commit: self.commit.clone(),
                detail: error.to_string(),
            })?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| BaselineError::UnreadableSource {
                file: directory.to_string(),
                path: self.root.clone(),
                commit: self.commit.clone(),
                detail: error.to_string(),
            })?;
            let file = entry.path();
            if file.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let Some(stem) = file.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let relative = format!("{directory}/{stem}");
            files.push((relative.clone(), self.read(&relative)?));
        }
        // Directory iteration order is filesystem-defined: sort so the matrix
        // never depends on it.
        files.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(files)
    }
}

/// HEAD of a clone, read-only. The clone is never written: every git invocation
/// in this crate is a query.
fn head_of(root: &Path, expected_commit: &str) -> Result<String, BaselineError> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| BaselineError::NotAGitRepository {
            path: root.to_path_buf(),
            expected_commit: expected_commit.to_string(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(BaselineError::NotAGitRepository {
            path: root.to_path_buf(),
            expected_commit: expected_commit.to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Rejects staged or unstaged changes to tracked files. Untracked files are
/// deliberately excluded: in particular, `.codex` is app-managed state and is
/// neither a normative source nor something this verifier may inspect.
fn ensure_tracked_clean(root: &Path, expected_commit: &str) -> Result<(), BaselineError> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "--untracked-files=no"])
        .output()
        .map_err(|error| BaselineError::NotAGitRepository {
            path: root.to_path_buf(),
            expected_commit: expected_commit.to_string(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(BaselineError::NotAGitRepository {
            path: root.to_path_buf(),
            expected_commit: expected_commit.to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let files = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if files.is_empty() {
        Ok(())
    } else {
        Err(BaselineError::DirtyTracked {
            path: root.to_path_buf(),
            expected_commit: expected_commit.to_string(),
            files,
        })
    }
}

/// One model row of the contract matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilityRow {
    pub slug: String,
    pub visibility: String,
    /// `direct` when the baseline omits `tool_mode`, which is how Codex reads it.
    pub tool_mode: String,
    pub responses_lite: bool,
    /// `disabled` when the baseline omits `multi_agent_version`.
    pub multi_agent_version: String,
}

/// Multi-agent surface for one protocol version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiAgentSurface {
    pub version: String,
    pub namespace: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppServerMethod {
    pub method: String,
    pub request: String,
    pub experimental: bool,
}

/// The versioned, deterministic contract matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityMatrix {
    pub schema_version: u32,
    pub baseline_commit: String,
    /// Baseline files the matrix is derived from, sorted.
    pub sources: Vec<String>,
    /// Tool modes the baseline can express (`direct`, `code_mode`, `code_mode_only`).
    pub tool_modes: Vec<String>,
    pub multi_agent_versions: Vec<String>,
    pub models: Vec<ModelCapabilityRow>,
    pub multi_agent: Vec<MultiAgentSurface>,
    pub app_server_methods: Vec<AppServerMethod>,
    /// SHA-256 of the matrix with this field emptied. Any drift changes it.
    pub fingerprint: String,
}

impl ParityMatrix {
    /// Extracts the matrix from a verified clone.
    pub fn extract(baseline: &CodexBaseline) -> Result<Self, BaselineError> {
        let models = extract_models(&baseline.read(MODELS_SOURCE)?)?;
        let tool_modes = extract_enum_variants(
            &baseline.read(TOOL_MODE_SOURCE)?,
            "ToolMode",
            TOOL_MODE_SOURCE,
        )?;
        let multi_agent_versions = extract_enum_variants(
            &baseline.read(MULTI_AGENT_VERSION_SOURCE)?,
            "MultiAgentVersion",
            MULTI_AGENT_VERSION_SOURCE,
        )?;
        let v1_namespace = extract_str_const(
            &baseline.read(MULTI_AGENT_V1_NAMESPACE_SOURCE)?,
            "MULTI_AGENT_V1_NAMESPACE",
            MULTI_AGENT_V1_NAMESPACE_SOURCE,
        )?;
        let v2_namespace = extract_str_const(
            &baseline.read(MULTI_AGENT_V2_NAMESPACE_SOURCE)?,
            "DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE",
            MULTI_AGENT_V2_NAMESPACE_SOURCE,
        )?;
        let v1_tools = extract_handler_tool_names(
            &baseline.read_dir_sources(MULTI_AGENT_V1_HANDLERS)?,
            MULTI_AGENT_V1_HANDLERS,
        )?;
        let v2_tools = extract_handler_tool_names(
            &baseline.read_dir_sources(MULTI_AGENT_V2_HANDLERS)?,
            MULTI_AGENT_V2_HANDLERS,
        )?;
        let app_server_methods =
            extract_app_server_methods(&baseline.read(APP_SERVER_SOURCE)?, APP_SERVER_SOURCE)?;

        let mut sources = vec![
            MODELS_SOURCE.to_string(),
            TOOL_MODE_SOURCE.to_string(),
            MULTI_AGENT_VERSION_SOURCE.to_string(),
            MULTI_AGENT_V1_NAMESPACE_SOURCE.to_string(),
            MULTI_AGENT_V2_NAMESPACE_SOURCE.to_string(),
            MULTI_AGENT_V1_HANDLERS.to_string(),
            MULTI_AGENT_V2_HANDLERS.to_string(),
            APP_SERVER_SOURCE.to_string(),
        ];
        sources.sort();
        sources.dedup();

        let mut matrix = Self {
            schema_version: MATRIX_SCHEMA_VERSION,
            baseline_commit: baseline.commit.clone(),
            sources,
            tool_modes,
            multi_agent_versions,
            models,
            multi_agent: vec![
                MultiAgentSurface {
                    version: "v1".into(),
                    namespace: v1_namespace,
                    tools: v1_tools,
                },
                MultiAgentSurface {
                    version: "v2".into(),
                    namespace: v2_namespace,
                    tools: v2_tools,
                },
            ],
            app_server_methods,
            fingerprint: String::new(),
        };
        matrix.fingerprint = matrix.compute_fingerprint();
        Ok(matrix)
    }

    /// SHA-256 over the matrix serialized with an empty fingerprint.
    pub fn compute_fingerprint(&self) -> String {
        let mut probe = self.clone();
        probe.fingerprint = String::new();
        // The struct is plain data: serialization cannot fail, and an empty
        // digest would be worse than a visible marker.
        let bytes = serde_json::to_vec(&probe).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    /// Human-readable differences against another matrix, empty when equal.
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        if self.schema_version != other.schema_version {
            differences.push(format!(
                "schema_version: {} -> {}",
                self.schema_version, other.schema_version
            ));
        }
        if self.baseline_commit != other.baseline_commit {
            differences.push(format!(
                "baseline_commit: {} -> {}",
                self.baseline_commit, other.baseline_commit
            ));
        }
        diff_lines("sources", &self.sources, &other.sources, &mut differences);
        diff_lines(
            "tool_modes",
            &self.tool_modes,
            &other.tool_modes,
            &mut differences,
        );
        diff_lines(
            "multi_agent_versions",
            &self.multi_agent_versions,
            &other.multi_agent_versions,
            &mut differences,
        );
        diff_lines(
            "models",
            &self.models.iter().map(model_line).collect::<Vec<_>>(),
            &other.models.iter().map(model_line).collect::<Vec<_>>(),
            &mut differences,
        );
        diff_lines(
            "multi_agent",
            &self
                .multi_agent
                .iter()
                .map(multi_agent_line)
                .collect::<Vec<_>>(),
            &other
                .multi_agent
                .iter()
                .map(multi_agent_line)
                .collect::<Vec<_>>(),
            &mut differences,
        );
        diff_lines(
            "app_server_methods",
            &self
                .app_server_methods
                .iter()
                .map(app_server_line)
                .collect::<Vec<_>>(),
            &other
                .app_server_methods
                .iter()
                .map(app_server_line)
                .collect::<Vec<_>>(),
            &mut differences,
        );
        differences
    }

    /// Stable pretty JSON, newline-terminated, for the committed file.
    pub fn to_pretty_json(&self) -> String {
        let mut rendered = serde_json::to_string_pretty(self).unwrap_or_default();
        rendered.push('\n');
        rendered
    }
}

fn model_line(model: &ModelCapabilityRow) -> String {
    format!(
        "{} visibility={} tool_mode={} responses_lite={} multi_agent={}",
        model.slug,
        model.visibility,
        model.tool_mode,
        model.responses_lite,
        model.multi_agent_version
    )
}

fn multi_agent_line(surface: &MultiAgentSurface) -> String {
    format!(
        "{} namespace={} tools=[{}]",
        surface.version,
        surface.namespace,
        surface.tools.join(",")
    )
}

fn app_server_line(method: &AppServerMethod) -> String {
    format!(
        "{} request={} experimental={}",
        method.method, method.request, method.experimental
    )
}

fn diff_lines(section: &str, left: &[String], right: &[String], out: &mut Vec<String>) {
    for line in left {
        if !right.contains(line) {
            out.push(format!("-{section}: {line}"));
        }
    }
    for line in right {
        if !left.contains(line) {
            out.push(format!("+{section}: {line}"));
        }
    }
}

#[derive(Deserialize)]
struct WireCatalog {
    models: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    slug: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    tool_mode: Option<String>,
    #[serde(default)]
    use_responses_lite: Option<bool>,
    #[serde(default)]
    multi_agent_version: Option<String>,
}

fn extract_models(body: &str) -> Result<Vec<ModelCapabilityRow>, BaselineError> {
    let catalog: WireCatalog =
        serde_json::from_str(body).map_err(|error| BaselineError::SourceDrift {
            file: MODELS_SOURCE.to_string(),
            detail: format!("expected an object with a models array: {error}"),
        })?;
    if catalog.models.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: MODELS_SOURCE.to_string(),
            detail: "model catalog is empty".to_string(),
        });
    }
    let mut models: Vec<ModelCapabilityRow> = catalog
        .models
        .into_iter()
        .map(|model| ModelCapabilityRow {
            slug: model.slug,
            visibility: model.visibility.unwrap_or_else(|| "list".to_string()),
            tool_mode: model.tool_mode.unwrap_or_else(|| "direct".to_string()),
            responses_lite: model.use_responses_lite.unwrap_or(false),
            multi_agent_version: model
                .multi_agent_version
                .unwrap_or_else(|| "disabled".to_string()),
        })
        .collect();
    models.sort_by(|left, right| left.slug.cmp(&right.slug));
    Ok(models)
}

/// Variants of `pub enum <name>`, in declaration order, as snake_case.
fn extract_enum_variants(
    body: &str,
    enum_name: &str,
    source: &str,
) -> Result<Vec<String>, BaselineError> {
    let header = format!("pub enum {enum_name} {{");
    let start = body
        .find(&header)
        .ok_or_else(|| BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("`{header}` not found"),
        })?
        + header.len();
    let block = &body[start..];
    let end = block
        .find("\n}")
        .ok_or_else(|| BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("unterminated `{enum_name}` body"),
        })?;
    let mut variants = Vec::new();
    for line in block[..end].lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        let Some(identifier) = line.strip_suffix(',') else {
            return Err(BaselineError::SourceDrift {
                file: source.to_string(),
                detail: format!("`{enum_name}` has a non-unit variant line: {line}"),
            });
        };
        if !identifier
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
            || identifier.is_empty()
        {
            return Err(BaselineError::SourceDrift {
                file: source.to_string(),
                detail: format!("`{enum_name}` has a non-unit variant: {identifier}"),
            });
        }
        variants.push(to_snake_case(identifier));
    }
    if variants.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("`{enum_name}` declares no variant"),
        });
    }
    Ok(variants)
}

fn to_snake_case(identifier: &str) -> String {
    let mut out = String::with_capacity(identifier.len() + 4);
    for (index, character) in identifier.char_indices() {
        if character.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.push(character.to_ascii_lowercase());
    }
    out
}

/// Value of a `const <name>: &str = "...";` declaration.
fn extract_str_const(body: &str, name: &str, source: &str) -> Result<String, BaselineError> {
    let needle = format!("{name}: &str = \"");
    let start = body
        .find(&needle)
        .ok_or_else(|| BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("`{name}` string constant not found"),
        })?
        + needle.len();
    let rest = &body[start..];
    let end = rest.find('"').ok_or_else(|| BaselineError::SourceDrift {
        file: source.to_string(),
        detail: format!("`{name}` string constant is unterminated"),
    })?;
    let value = &rest[..end];
    if value.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("`{name}` is empty"),
        });
    }
    Ok(value.to_string())
}

/// Tool name declared by each handler module, from the first `ToolName::plain`
/// or `ToolName::namespaced` call in the file. Modules without one are helpers.
fn extract_handler_tool_names(
    files: &[(String, String)],
    directory: &str,
) -> Result<Vec<String>, BaselineError> {
    if files.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: directory.to_string(),
            detail: "handler directory contains no Rust module".to_string(),
        });
    }
    let mut tools = Vec::new();
    for (_, body) in files {
        if let Some(name) = first_tool_name(body) {
            tools.push(name);
        }
    }
    if tools.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: directory.to_string(),
            detail: "no handler declares a ToolName".to_string(),
        });
    }
    tools.sort();
    tools.dedup();
    Ok(tools)
}

fn first_tool_name(body: &str) -> Option<String> {
    let plain = body.find("ToolName::plain(\"");
    let namespaced = body.find("ToolName::namespaced(");
    match (plain, namespaced) {
        (Some(plain_at), Some(namespaced_at)) if plain_at < namespaced_at => {
            quoted_after(body, plain_at + "ToolName::plain(\"".len() - 1)
        }
        (Some(plain_at), None) => quoted_after(body, plain_at + "ToolName::plain(\"".len() - 1),
        (_, Some(namespaced_at)) => {
            // `namespaced(NAMESPACE_CONST, "tool")`: the tool name is the only
            // quoted argument.
            let rest = &body[namespaced_at..];
            let quote = rest.find('"')?;
            quoted_after(rest, quote)
        }
        (None, None) => None,
    }
}

/// Reads the string literal starting at the opening quote `at`.
fn quoted_after(body: &str, at: usize) -> Option<String> {
    let rest = body.get(at + 1..)?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_string())
}

/// App-server request methods declared by `client_request_definitions!`.
fn extract_app_server_methods(
    body: &str,
    source: &str,
) -> Result<Vec<AppServerMethod>, BaselineError> {
    let header = "client_request_definitions! {";
    let start = body
        .find(header)
        .ok_or_else(|| BaselineError::SourceDrift {
            file: source.to_string(),
            detail: format!("`{header}` not found"),
        })?
        + header.len();
    let block = &body[start..];
    let end = block
        .find("\n}")
        .ok_or_else(|| BaselineError::SourceDrift {
            file: source.to_string(),
            detail: "unterminated client_request_definitions! block".to_string(),
        })?;

    let mut methods: BTreeMap<String, AppServerMethod> = BTreeMap::new();
    let mut experimental = false;
    for line in block[..end].lines() {
        let line = line.trim();
        if line.starts_with("#[experimental(") {
            experimental = true;
            continue;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            continue;
        }
        let Some((request, rest)) = line.split_once("=>") else {
            continue;
        };
        let request = request.trim();
        if request.is_empty() || !request.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let rest = rest.trim();
        let Some(method) = rest
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
        else {
            continue;
        };
        if method.is_empty() {
            continue;
        }
        if let Some(previous) = methods.insert(
            method.to_string(),
            AppServerMethod {
                method: method.to_string(),
                request: request.to_string(),
                experimental,
            },
        ) {
            return Err(BaselineError::SourceDrift {
                file: source.to_string(),
                detail: format!(
                    "method {method} declared twice ({} and {request})",
                    previous.request
                ),
            });
        }
        experimental = false;
    }
    if methods.is_empty() {
        return Err(BaselineError::SourceDrift {
            file: source.to_string(),
            detail: "client_request_definitions! declares no method".to_string(),
        });
    }
    Ok(methods.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_clone_reports_expected_path_and_commit() {
        let error = CodexBaseline::open("/nonexistent/codex-baseline").unwrap_err();
        let rendered = error.to_string();
        assert!(
            rendered.contains("/nonexistent/codex-baseline"),
            "{rendered}"
        );
        assert!(rendered.contains(BASELINE_COMMIT), "{rendered}");
        assert!(matches!(error, BaselineError::MissingClone { .. }));
    }

    #[test]
    fn wrong_commit_reports_both_commits_and_does_not_extract() {
        // The Pyxis repository itself is a valid git clone at another commit.
        let error = CodexBaseline::open(env!("CARGO_MANIFEST_DIR")).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains(BASELINE_COMMIT), "{rendered}");
        assert!(matches!(error, BaselineError::CommitMismatch { .. }));
    }

    #[test]
    fn pinned_verification_rejects_tracked_dirt_but_ignores_untracked_codex_state() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("agent-parity-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "parity@example.invalid"]);
        git(&["config", "user.name", "Parity Test"]);
        std::fs::write(root.join("tracked.rs"), "clean\n").unwrap();
        git(&["add", "tracked.rs"]);
        git(&["commit", "-q", "-m", "fixture"]);
        let commit = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(root.join(".codex/hooks.json"), "do not inspect").unwrap();
        CodexBaseline::open_at_commit(&root, &commit)
            .expect("untracked app state is outside the baseline");

        std::fs::write(root.join("tracked.rs"), "dirty\n").unwrap();
        let error = CodexBaseline::open_at_commit(&root, &commit).unwrap_err();
        let rendered = error.to_string();
        assert!(matches!(error, BaselineError::DirtyTracked { .. }));
        assert!(rendered.contains(&root.display().to_string()), "{rendered}");
        assert!(rendered.contains(&commit), "{rendered}");
        assert!(rendered.contains("tracked.rs"), "{rendered}");
        assert_eq!(
            std::fs::read_to_string(root.join(".codex/hooks.json")).unwrap(),
            "do not inspect"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// US-020 AC4: the drift report reads a clone at ANY commit, so an upstream
    /// HEAD can be compared without moving the baseline. `open` still refuses
    /// the same clone, which is what keeps the two modes from collapsing into
    /// one.
    #[test]
    fn the_drift_entry_point_accepts_a_clone_at_another_commit() {
        // The Pyxis repository is a valid clone at a commit that is not the
        // Codex baseline.
        let root = env!("CARGO_MANIFEST_DIR");
        let unpinned = CodexBaseline::open_unpinned(root).expect("an unpinned clone opens");
        assert_ne!(unpinned.commit(), BASELINE_COMMIT);
        assert!(!unpinned.commit().is_empty());
        assert!(matches!(
            CodexBaseline::open(root),
            Err(BaselineError::CommitMismatch { .. })
        ));
    }

    /// A missing clone is still a missing clone, drift or not: the report names
    /// the path and the baseline it was going to compare against.
    #[test]
    fn the_drift_entry_point_still_reports_a_missing_clone() {
        let error = CodexBaseline::open_unpinned("/nonexistent/codex-baseline").unwrap_err();
        assert!(matches!(error, BaselineError::MissingClone { .. }));
        assert!(error.to_string().contains(BASELINE_COMMIT));
    }

    #[test]
    fn models_default_absent_capabilities_the_way_codex_reads_them() {
        let rows = extract_models(
            r#"{"models":[
                {"slug":"z-model","visibility":"list","tool_mode":"code_mode_only",
                 "use_responses_lite":true,"multi_agent_version":"v2"},
                {"slug":"a-model"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(rows[0].slug, "a-model", "rows are sorted by slug");
        assert_eq!(rows[0].tool_mode, "direct");
        assert!(!rows[0].responses_lite);
        assert_eq!(rows[0].multi_agent_version, "disabled");
        assert_eq!(rows[1].tool_mode, "code_mode_only");
        assert!(rows[1].responses_lite);
        assert_eq!(rows[1].multi_agent_version, "v2");
    }

    #[test]
    fn empty_or_malformed_catalog_is_drift_not_an_empty_section() {
        assert!(matches!(
            extract_models(r#"{"models":[]}"#),
            Err(BaselineError::SourceDrift { .. })
        ));
        assert!(matches!(
            extract_models("{"),
            Err(BaselineError::SourceDrift { .. })
        ));
    }

    #[test]
    fn enum_variants_are_snake_cased_in_declaration_order() {
        let body = r#"
#[derive(Debug)]
pub enum ToolMode {
    Direct,
    // comment
    CodeMode,
    CodeModeOnly,
}
"#;
        assert_eq!(
            extract_enum_variants(body, "ToolMode", "test").unwrap(),
            ["direct", "code_mode", "code_mode_only"]
        );
        assert_eq!(
            extract_enum_variants(
                "pub enum MultiAgentVersion {\n    Disabled,\n    V1,\n    V2,\n}\n",
                "MultiAgentVersion",
                "test"
            )
            .unwrap(),
            ["disabled", "v1", "v2"]
        );
    }

    #[test]
    fn renamed_or_restructured_enum_fails_with_the_source() {
        assert!(matches!(
            extract_enum_variants("pub enum Other {\n    A,\n}\n", "ToolMode", "openai_models.rs"),
            Err(BaselineError::SourceDrift { file, .. }) if file == "openai_models.rs"
        ));
        assert!(matches!(
            extract_enum_variants(
                "pub enum ToolMode {\n    Direct { legacy: bool },\n}\n",
                "ToolMode",
                "test"
            ),
            Err(BaselineError::SourceDrift { .. })
        ));
    }

    #[test]
    fn namespace_constants_are_read_from_source() {
        let body = "pub const MULTI_AGENT_V1_NAMESPACE: &str = \"multi_agent_v1\";\n";
        assert_eq!(
            extract_str_const(body, "MULTI_AGENT_V1_NAMESPACE", "test").unwrap(),
            "multi_agent_v1"
        );
        assert!(matches!(
            extract_str_const(body, "MISSING", "test"),
            Err(BaselineError::SourceDrift { .. })
        ));
    }

    #[test]
    fn handler_tool_names_come_from_the_first_tool_name_call() {
        let files = vec![
            (
                "dir/spawn.rs".to_string(),
                "fn name(&self) -> ToolName {\n    ToolName::plain(\"spawn_agent\")\n}\n"
                    .to_string(),
            ),
            (
                "dir/close_agent.rs".to_string(),
                "ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, \"close_agent\")".to_string(),
            ),
            ("dir/message_tool.rs".to_string(), "// helper".to_string()),
        ];
        assert_eq!(
            extract_handler_tool_names(&files, "dir").unwrap(),
            ["close_agent", "spawn_agent"]
        );
        assert!(matches!(
            extract_handler_tool_names(&[], "dir"),
            Err(BaselineError::SourceDrift { .. })
        ));
    }

    #[test]
    fn app_server_methods_keep_their_experimental_flag() {
        let body = r#"
client_request_definitions! {
    Initialize => "initialize" {
        params: v1::InitializeParams,
    },
    #[experimental("thread/increment_elicitation")]
    /// doc
    ThreadIncrementElicitation => "thread/increment_elicitation" {
        params: v2::Params,
    },
    ThreadStart => "thread/start" {
        params: v2::ThreadStartParams,
    },
}
"#;
        let methods = extract_app_server_methods(body, "common.rs").unwrap();
        assert_eq!(
            methods
                .iter()
                .map(|method| method.method.as_str())
                .collect::<Vec<_>>(),
            ["initialize", "thread/increment_elicitation", "thread/start"]
        );
        assert!(!methods[0].experimental);
        assert!(methods[1].experimental);
        assert!(!methods[2].experimental, "the flag applies to one method");
        assert_eq!(methods[2].request, "ThreadStart");
    }

    #[test]
    fn duplicate_app_server_method_is_drift() {
        let body = "client_request_definitions! {\n    A => \"m\" {\n    },\n    B => \"m\" {\n    },\n}\n";
        assert!(matches!(
            extract_app_server_methods(body, "common.rs"),
            Err(BaselineError::SourceDrift { .. })
        ));
    }

    #[test]
    fn fingerprint_covers_every_section_and_ignores_itself() {
        let mut matrix = ParityMatrix {
            schema_version: MATRIX_SCHEMA_VERSION,
            baseline_commit: BASELINE_COMMIT.to_string(),
            sources: vec!["a".into()],
            tool_modes: vec!["direct".into()],
            multi_agent_versions: vec!["v2".into()],
            models: Vec::new(),
            multi_agent: Vec::new(),
            app_server_methods: Vec::new(),
            fingerprint: String::new(),
        };
        matrix.fingerprint = matrix.compute_fingerprint();
        let stable = matrix.clone();
        assert_eq!(stable.compute_fingerprint(), matrix.fingerprint);

        let mut drifted = matrix.clone();
        drifted.tool_modes.push("code_mode_only".into());
        assert_ne!(drifted.compute_fingerprint(), matrix.fingerprint);
        assert_eq!(matrix.diff(&drifted), ["+tool_modes: code_mode_only"]);
    }

    /// The committed matrix is the normative artifact: it must stay loadable,
    /// self-consistent and aligned with the baseline the PRD pins, with or
    /// without a Codex clone on the machine.
    #[test]
    fn committed_matrix_is_self_consistent() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(COMMITTED_MATRIX_PATH);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
        let matrix: ParityMatrix = serde_json::from_str(&body).expect("committed matrix parses");
        assert_eq!(matrix.schema_version, MATRIX_SCHEMA_VERSION);
        assert_eq!(matrix.baseline_commit, BASELINE_COMMIT);
        assert_eq!(matrix.fingerprint, matrix.compute_fingerprint());
        assert_eq!(matrix.to_pretty_json(), body, "committed file is canonical");

        assert!(matrix.tool_modes.contains(&"code_mode_only".to_string()));
        assert!(matrix.multi_agent_versions.contains(&"v2".to_string()));
        let sol = matrix
            .models
            .iter()
            .find(|model| model.slug == "gpt-5.6-sol")
            .expect("baseline frontier model");
        assert_eq!(sol.tool_mode, "code_mode_only");
        assert_eq!(sol.multi_agent_version, "v2");
        assert!(sol.responses_lite);
        let v2 = matrix
            .multi_agent
            .iter()
            .find(|surface| surface.version == "v2")
            .expect("v2 surface");
        assert_eq!(
            v2.tools,
            [
                "followup_task",
                "interrupt_agent",
                "list_agents",
                "send_message",
                "spawn_agent",
                "wait_agent"
            ]
        );
        assert!(
            matrix
                .app_server_methods
                .iter()
                .any(|method| method.method == "thread/start")
        );
    }

    /// Runs only where the pinned clone exists: proves extraction is
    /// deterministic and still matches the committed matrix.
    #[test]
    fn extraction_matches_the_committed_matrix_when_the_clone_is_present() {
        let Ok(baseline) = CodexBaseline::from_env() else {
            return;
        };
        let first = ParityMatrix::extract(&baseline).expect("matrix extracts");
        let second = ParityMatrix::extract(&baseline).expect("matrix extracts");
        assert_eq!(first, second, "extraction is deterministic");

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(COMMITTED_MATRIX_PATH);
        let committed: ParityMatrix =
            serde_json::from_str(&std::fs::read_to_string(path).expect("committed matrix"))
                .expect("committed matrix parses");
        assert_eq!(
            committed.diff(&first),
            Vec::<String>::new(),
            "regenerate with `cargo run -p agent-parity -- generate`"
        );
    }
}
