//! Outil `grep` — recherche une regex dans les fichiers du workspace et retourne
//! les correspondances `chemin:ligne: contenu`. Read-only, concurrency-safe.
//! US-011 AC2.

use async_trait::async_trait;
use globset::Glob as GlobPattern;
use regex::Regex;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::error::{ToolError, ValidationError};
use crate::path::{confine, ensure_existing_path_no_links};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

const MAX_MATCHES: usize = 500;
/// Fichiers plus gros que ça sont ignorés (probablement des artefacts).
const MAX_FILE_BYTES: u64 = 5_000_000;
/// Borne d'affichage d'une ligne de correspondance (évite un flood sur une ligne
/// minifiée). Coupe sur une frontière de caractère (cf. `truncate_line`).
const MAX_LINE_BYTES: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    /// Expression régulière (syntaxe `regex`).
    pub pattern: String,
    /// Sous-dossier ou fichier de base (relatif au workspace). Défaut : racine.
    #[serde(default)]
    pub path: Option<String>,
    /// Filtre les fichiers parcourus par un motif glob (ex. "*.rs").
    #[serde(default)]
    pub glob: Option<String>,
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    type Input = GrepInput;

    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> String {
        "Recherche une expression régulière dans les fichiers du workspace et \
         retourne les correspondances au format chemin:ligne: contenu. \
         Paramètres : pattern (regex), path (base, optionnel), glob (filtre de \
         noms de fichiers, optionnel)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Expression régulière." },
                "path": { "type": ["string", "null"], "description": "Base de recherche (relative au workspace), ou null." },
                "glob": { "type": ["string", "null"], "description": "Filtre glob sur les noms de fichiers, ex. *.rs, ou null." }
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
        Regex::new(&input.pattern)
            .map(|_| ())
            .map_err(|e| ValidationError::new(format!("regex invalide: {e}")))?;
        if let Some(g) = &input.glob {
            GlobPattern::new(g)
                .map(|_| ())
                .map_err(|e| ValidationError::new(format!("motif glob invalide: {e}")))?;
        }
        Ok(())
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let re = Regex::new(&input.pattern)
            .map_err(|e| ToolError::Rejected(format!("regex invalide: {e}")))?;
        let name_filter = match &input.glob {
            Some(g) => Some(
                GlobPattern::new(g)
                    .map_err(|e| ToolError::Rejected(format!("motif glob invalide: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let base = match &input.path {
            Some(p) => confine(&ctx.workspace, p)?,
            None => ctx.workspace.clone(),
        };
        ensure_existing_path_no_links(&ctx.workspace, &base, input.path.as_deref().unwrap_or("."))?;
        let workspace = ctx.workspace.clone();

        let (lines, truncated) = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            let mut truncated = false;
            'walk: for entry in WalkDir::new(&base).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Some(f) = &name_filter {
                    let fname = entry.file_name();
                    if !f.is_match(fname) {
                        continue;
                    }
                }
                if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                    continue;
                }
                let bytes = match std::fs::read(entry.path()) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if bytes.contains(&0) {
                    continue; // binaire
                }
                let text = String::from_utf8_lossy(&bytes);
                let rel = entry
                    .path()
                    .strip_prefix(&workspace)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                for (idx, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let trimmed = truncate_line(line, MAX_LINE_BYTES);
                        out.push(format!("{}:{}: {}", rel, idx + 1, trimmed));
                        if out.len() >= MAX_MATCHES {
                            truncated = true;
                            break 'walk;
                        }
                    }
                }
            }
            (out, truncated)
        })
        .await
        .map_err(|e| ToolError::Io(format!("walk: {e}")))?;

        if lines.is_empty() {
            return Ok(ToolOutput::text(format!(
                "(aucune correspondance pour « {} »)",
                input.pattern
            )));
        }
        let mut body = lines.join("\n");
        if truncated {
            // US-026 : signaler la troncation ET le moyen de paginer (grep n'a pas
            // d'offset → on guide vers un resserrage de la recherche).
            body.push_str(&format!(
                "\n[truncated: {MAX_MATCHES} correspondances atteintes — affinez avec un \
                 pattern plus précis, un glob, ou un path plus étroit pour voir la suite]"
            ));
        }
        Ok(ToolOutput::text(body))
    }
}

/// Tronque une ligne d'affichage à `max` OCTETS sur une frontière de caractère
/// UTF-8. Indispensable : `&line[..max]` panique si l'octet `max` tombe au milieu
/// d'un codepoint multi-octets (ligne à accents/CJK > `max` octets) — récurrent sur
/// du source à commentaires français. On recule jusqu'à la frontière la plus proche.
fn truncate_line(line: &str, max: usize) -> &str {
    if line.len() <= max {
        return line;
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_line_is_untouched() {
        assert_eq!(truncate_line("court", MAX_LINE_BYTES), "court");
    }

    #[test]
    fn long_ascii_line_is_cut_at_max() {
        let line = "x".repeat(400);
        assert_eq!(truncate_line(&line, MAX_LINE_BYTES).len(), MAX_LINE_BYTES);
    }

    #[test]
    fn multibyte_boundary_does_not_panic_and_stays_valid_utf8() {
        // "a" + 150 × 'é' = 301 octets : l'octet 300 tombe au MILIEU du 150ᵉ 'é'
        // → `&line[..300]` paniquerait. La coupe recule sur la frontière (299).
        let line = format!("a{}", "é".repeat(150));
        assert!(line.len() > MAX_LINE_BYTES && !line.is_char_boundary(MAX_LINE_BYTES));
        let cut = truncate_line(&line, MAX_LINE_BYTES);
        assert!(cut.len() <= MAX_LINE_BYTES, "borné");
        assert!(
            line.starts_with(cut),
            "préfixe valide, aucune coupe mid-codepoint"
        );
    }
}
