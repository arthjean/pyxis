//! Outil `glob` — liste les fichiers du workspace correspondant à un motif glob
//! (`**/*.rs`, `src/*.toml`, …). Read-only, concurrency-safe. US-011 AC2.

use async_trait::async_trait;
use globset::Glob as GlobPattern;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::error::{ToolError, ValidationError};
use crate::path::{confine, ensure_existing_path_no_links};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Plafond de résultats (évite un flood de prompt sur un repo géant).
const MAX_MATCHES: usize = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    pub pattern: String,
    /// Sous-dossier de base (relatif au workspace). Défaut : racine workspace.
    #[serde(default)]
    pub path: Option<String>,
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    type Input = GlobInput;

    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> String {
        "Liste les fichiers du workspace correspondant à un motif glob (ex. \
         \"**/*.rs\", \"src/*.toml\"). Paramètres : pattern (le motif), path \
         (sous-dossier de base, optionnel). Chemins retournés relatifs au \
         workspace."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Motif glob, ex. **/*.rs" },
                "path": { "type": ["string", "null"], "description": "Sous-dossier de base (relatif au workspace), ou null." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn validate_input(&self, input: &Self::Input) -> Result<(), ValidationError> {
        GlobPattern::new(&input.pattern)
            .map(|_| ())
            .map_err(|e| ValidationError::new(format!("motif glob invalide: {e}")))
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let matcher = GlobPattern::new(&input.pattern)
            .map_err(|e| ToolError::Rejected(format!("motif glob invalide: {e}")))?
            .compile_matcher();
        let base = match &input.path {
            Some(p) => confine(&ctx.workspace, p)?,
            None => ctx.workspace.clone(),
        };
        ensure_existing_path_no_links(&ctx.workspace, &base, input.path.as_deref().unwrap_or("."))?;
        let workspace = ctx.workspace.clone();
        let pattern = input.pattern.clone();

        // Walk synchrone (FS bloquant) déporté hors du runtime async.
        let matches = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            for entry in WalkDir::new(&base).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&workspace)
                    .unwrap_or(entry.path());
                if matcher.is_match(rel) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                    if out.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            out.sort();
            out
        })
        .await
        .map_err(|e| ToolError::Io(format!("walk: {e}")))?;

        if matches.is_empty() {
            return Ok(ToolOutput::text(format!(
                "(aucun fichier ne correspond à « {pattern} »)"
            )));
        }
        let mut body = matches.join("\n");
        if matches.len() >= MAX_MATCHES {
            body.push_str(&format!("\n… (tronqué à {MAX_MATCHES} résultats)"));
        }
        Ok(ToolOutput::text(body))
    }
}
