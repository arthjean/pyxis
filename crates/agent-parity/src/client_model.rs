//! Declarative residual client/model/provider contract for the 2026-08-01 baseline.
//!
//! The committed JSON is the audit record. This module validates that record
//! against the pinned Codex clone and existing Pyxis proofs; it does not pretend
//! that human contract axes can be reconstructed from source-name substrings.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CodexBaseline;

pub const BASELINE_COMMIT: &str = "ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff";
pub const MATRIX_SCHEMA_VERSION: u32 = 1;
pub const COMMITTED_MATRIX_PATH: &str = "docs/parity/codex-client-model-matrix.json";
pub const GAP_FAMILY_COUNT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceContract {
    pub file: String,
    pub anchors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapFamily {
    pub id: String,
    pub title: String,
    pub sources: Vec<SourceContract>,
    pub input: String,
    pub output: String,
    pub state: String,
    pub error: String,
    pub edge_case: String,
    pub expected_proof: String,
    /// Repository paths of the artifacts that already prove part of this family.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub existing_proofs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientModelMatrix {
    pub schema_version: u32,
    pub baseline_commit: String,
    pub sources: Vec<String>,
    pub gap_families: Vec<GapFamily>,
    pub fingerprint: String,
}

impl ClientModelMatrix {
    /// Validates the human audit record and every Pyxis proof it references.
    pub fn validate(&self, repository_root: &Path) -> Result<(), String> {
        if self.schema_version != MATRIX_SCHEMA_VERSION {
            return Err(format!(
                "matrix schema {} is unsupported, expected {MATRIX_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        if self.baseline_commit != BASELINE_COMMIT {
            return Err(format!(
                "matrix baseline {} does not match {BASELINE_COMMIT}",
                self.baseline_commit
            ));
        }
        if self.gap_families.len() != GAP_FAMILY_COUNT {
            return Err(format!(
                "matrix contains {} gap families, expected {GAP_FAMILY_COUNT}",
                self.gap_families.len()
            ));
        }

        let mut ids = BTreeSet::new();
        let mut declared_sources = BTreeSet::new();
        for family in &self.gap_families {
            validate_family(family)?;
            if !ids.insert(family.id.as_str()) {
                return Err(format!("duplicate gap family `{}`", family.id));
            }
            for source in &family.sources {
                if source.file.split('/').any(|part| part == ".codex") {
                    return Err(format!(
                        "family {} uses forbidden app-managed source {}",
                        family.id, source.file
                    ));
                }
                if source.anchors.is_empty() || source.anchors.iter().any(String::is_empty) {
                    return Err(format!(
                        "family {} source {} has no complete anchor set",
                        family.id, source.file
                    ));
                }
                declared_sources.insert(source.file.as_str());
            }
            for proof in &family.existing_proofs {
                validate_existing_proof(repository_root, family, proof)?;
            }
        }

        let listed_sources: BTreeSet<&str> = self.sources.iter().map(String::as_str).collect();
        if listed_sources != declared_sources || listed_sources.len() != self.sources.len() {
            return Err(
                "matrix sources do not exactly match the family source declarations".into(),
            );
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err("matrix fingerprint does not cover its current content".into());
        }
        Ok(())
    }

    /// Verifies the declarative audit against the immutable baseline checkout.
    pub fn verify_baseline(&self, baseline: &CodexBaseline) -> Result<(), String> {
        if baseline.commit() != BASELINE_COMMIT {
            return Err(format!(
                "{} is at {}, expected {BASELINE_COMMIT}",
                baseline.root().display(),
                baseline.commit()
            ));
        }
        for family in &self.gap_families {
            for source in &family.sources {
                let path = baseline.root().join(&source.file);
                let body = std::fs::read_to_string(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                for anchor in &source.anchors {
                    if !body.contains(anchor) {
                        return Err(format!(
                            "family {} does not find `{anchor}` in {}",
                            family.id, source.file
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn refresh_fingerprint(&mut self) {
        self.fingerprint = self.compute_fingerprint();
    }

    pub fn compute_fingerprint(&self) -> String {
        let mut probe = self.clone();
        probe.fingerprint.clear();
        let bytes = serde_json::to_vec(&probe).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    pub fn to_pretty_json(&self) -> String {
        let mut rendered = serde_json::to_string_pretty(self).unwrap_or_default();
        rendered.push('\n');
        rendered
    }
}

fn validate_family(family: &GapFamily) -> Result<(), String> {
    let axes = [
        ("id", family.id.as_str()),
        ("title", family.title.as_str()),
        ("input", family.input.as_str()),
        ("output", family.output.as_str()),
        ("state", family.state.as_str()),
        ("error", family.error.as_str()),
        ("edge_case", family.edge_case.as_str()),
        ("expected_proof", family.expected_proof.as_str()),
    ];
    if let Some((axis, _)) = axes.into_iter().find(|(_, value)| value.trim().is_empty()) {
        return Err(format!("family {} has an empty {axis} axis", family.id));
    }
    if family.sources.is_empty() {
        return Err(format!("family {} has no normative source", family.id));
    }
    Ok(())
}

fn validate_existing_proof(
    repository_root: &Path,
    family: &GapFamily,
    proof: &str,
) -> Result<(), String> {
    if proof.trim().is_empty() {
        return Err(format!(
            "family {} names an empty proof artifact",
            family.id
        ));
    }
    let artifact = repository_root.join(proof);
    if !artifact.exists() {
        return Err(format!(
            "family {} proof artifact is missing: {}",
            family.id,
            artifact.display()
        ));
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<ClientModelMatrix, String> {
    let body = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&body).map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

pub fn load_committed(root: &Path) -> Result<ClientModelMatrix, String> {
    load(&root.join(COMMITTED_MATRIX_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn committed_matrix_is_structurally_complete_and_canonical() {
        let root = repository_root();
        let matrix = load_committed(&root).unwrap();
        matrix.validate(&root).unwrap();
        assert_eq!(
            matrix.to_pretty_json(),
            std::fs::read_to_string(root.join(COMMITTED_MATRIX_PATH)).unwrap()
        );
    }

    #[test]
    fn committed_matrix_matches_the_pinned_clone_when_present() {
        let Ok(baseline) =
            CodexBaseline::open_at_commit(crate::DEFAULT_BASELINE_PATH, BASELINE_COMMIT)
        else {
            return;
        };
        let root = repository_root();
        let matrix = load_committed(&root).unwrap();
        matrix.validate(&root).unwrap();
        matrix.verify_baseline(&baseline).unwrap();
    }
}
