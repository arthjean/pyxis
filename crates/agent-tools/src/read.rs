//! Outil `read` — lit un fichier du workspace avec numéros de ligne. Read-only,
//! concurrency-safe, sortie untrusted (le contenu lu peut porter une injection,
//! OWASP LLM01). US-011 AC1/AC3.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ToolError;
use crate::path::confine;
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Au-delà, on considère le contenu binaire/illisible (présence d'octets NUL
/// vérifiée séparément ; ceci borne juste la taille lue en MVP).
const MAX_BYTES: usize = 2_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
    /// Ligne de départ (1-indexée). Défaut : 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Nombre de lignes max. Défaut : tout.
    #[serde(default)]
    pub limit: Option<usize>,
}

pub struct Read;

#[async_trait]
impl Tool for Read {
    type Input = ReadInput;

    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> String {
        "Lit un fichier texte du workspace et retourne son contenu préfixé des \
         numéros de ligne. Paramètres : path (relatif au workspace), offset \
         (ligne de départ 1-indexée, optionnel), limit (nombre de lignes, \
         optionnel)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Chemin du fichier (relatif au workspace)." },
                "offset": { "type": ["integer", "null"], "minimum": 1, "description": "Ligne de départ (1-indexée), ou null." },
                "limit": { "type": ["integer", "null"], "minimum": 1, "description": "Nombre de lignes maximum, ou null." }
            },
            "required": ["path", "offset", "limit"],
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
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        if meta.is_dir() {
            return Err(ToolError::Rejected(format!(
                "{} est un répertoire, pas un fichier",
                input.path
            )));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        if bytes.contains(&0) {
            return Err(ToolError::Rejected(format!(
                "{} semble être un fichier binaire (octets NUL)",
                input.path
            )));
        }
        // US-026 : au-delà de MAX_BYTES, lecture PARTIELLE (tête du fichier, coupée
        // sur une frontière de caractère) + hint de pagination, au lieu d'un rejet sec.
        let full = String::from_utf8_lossy(&bytes);
        let oversize = bytes.len() > MAX_BYTES;
        let text: &str = if oversize {
            let mut cut = MAX_BYTES;
            while cut > 0 && !full.is_char_boundary(cut) {
                cut -= 1;
            }
            &full[..cut]
        } else {
            full.as_ref()
        };
        let start = input.offset.unwrap_or(1).max(1);
        Ok(ToolOutput::text(render_read(
            text,
            start,
            input.limit,
            oversize,
        )))
    }
}

/// Rend les lignes numérotées de `text` depuis `start` (1-indexé), au plus `limit`
/// lignes, avec des HINTS de continuation (US-026) : limite atteinte →
/// `[lignes X-Y sur Z ; offset=Y+1 pour continuer]` ; `oversize` → hint de lecture
/// partielle ; plage hors limites → hint plutôt qu'un message vague. Pur → testable
/// sans I/O.
fn render_read(text: &str, start: usize, limit: Option<usize>, oversize: bool) -> String {
    let total = text.lines().count();
    let mut out = String::new();
    let mut emitted = 0usize;
    let mut last_line = 0usize;
    let mut truncated_by_limit = false;
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        if lineno < start {
            continue;
        }
        if limit.is_some_and(|l| emitted >= l) {
            truncated_by_limit = true;
            break;
        }
        out.push_str(&format!("{lineno:>6}\t{line}\n"));
        emitted += 1;
        last_line = lineno;
    }
    if out.is_empty() {
        if total == 0 {
            out.push_str("(fichier vide)");
        } else {
            out.push_str(&format!(
                "[plage hors limites : offset={start} > {total} lignes]"
            ));
        }
        return out;
    }
    if oversize {
        out.push_str(&format!(
            "[fichier tronqué à {MAX_BYTES} octets ({emitted} lignes lues) — lisez par \
             plages avec offset/limit]"
        ));
    } else if truncated_by_limit {
        out.push_str(&format!(
            "[lignes {start}-{last_line} sur {total} ; offset={} pour continuer]",
            last_line + 1
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text5() -> &'static str {
        "l1\nl2\nl3\nl4\nl5\n"
    }

    #[test]
    fn full_read_has_no_hint() {
        let out = render_read(text5(), 1, None, false);
        assert!(out.contains("     1\tl1"));
        assert!(out.contains("     5\tl5"));
        assert!(
            !out.contains("offset="),
            "lecture complète → pas de hint: {out}"
        );
    }

    #[test]
    fn limit_truncation_emits_continuation_hint() {
        // 5 lignes, limit 2 depuis offset 1 → lignes 1-2, hint offset=3.
        let out = render_read(text5(), 1, Some(2), false);
        assert!(out.contains("     1\tl1"));
        assert!(out.contains("     2\tl2"));
        assert!(!out.contains("\tl3"));
        assert!(
            out.contains("[lignes 1-2 sur 5 ; offset=3 pour continuer]"),
            "hint de pagination attendu: {out}"
        );
    }

    #[test]
    fn out_of_range_offset_hints_instead_of_vague_message() {
        let out = render_read(text5(), 99, None, false);
        assert!(
            out.contains("[plage hors limites : offset=99 > 5 lignes]"),
            "hint hors-plage attendu: {out}"
        );
    }

    #[test]
    fn oversize_emits_partial_read_hint() {
        let out = render_read("a\nb\n", 1, None, true);
        assert!(out.contains("     1\ta"));
        assert!(
            out.contains("fichier tronqué à") && out.contains("lisez par plages"),
            "hint de lecture partielle attendu: {out}"
        );
    }

    #[test]
    fn empty_file_reports_empty() {
        assert_eq!(render_read("", 1, None, false), "(fichier vide)");
    }
}
