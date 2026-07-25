//! `grep` tool: searches a regex in the workspace files and returns
//! the `path:line: content` matches. Read-only, concurrency-safe.
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
/// Files larger than this are skipped (most likely artifacts).
const MAX_FILE_BYTES: u64 = 5_000_000;
/// Display bound of a match line (avoids flooding on a minified
/// line). Cuts on a character boundary (see `truncate_line`).
const MAX_LINE_BYTES: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    /// Regular expression (`regex` syntax).
    pub pattern: String,
    /// Base subdirectory or file (relative to the workspace). Default: root.
    #[serde(default)]
    pub path: Option<String>,
    /// Filters the walked files by a glob pattern (e.g. "*.rs").
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
        "Search for a regular expression in workspace files and return matches \
         as path:line: content. Parameters: pattern (regex), path (optional base), \
         glob (optional filename filter)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression." },
                "path": { "type": ["string", "null"], "description": "Search base relative to the workspace, or null." },
                "glob": { "type": ["string", "null"], "description": "Filename glob filter, for example *.rs, or null." }
            },
            "required": ["pattern", "path", "glob"],
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
    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        Regex::new(&input.pattern)
            .map(|_| ())
            .map_err(|e| ValidationError::new(format!("invalid regex: {e}")))?;
        if let Some(g) = &input.glob {
            GlobPattern::new(g)
                .map(|_| ())
                .map_err(|e| ValidationError::new(format!("invalid glob pattern: {e}")))?;
        }
        Ok(())
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let re = Regex::new(&input.pattern)
            .map_err(|e| ToolError::Rejected(format!("invalid regex: {e}")))?;
        let name_filter = match &input.glob {
            Some(g) => Some(
                GlobPattern::new(g)
                    .map_err(|e| ToolError::Rejected(format!("invalid glob pattern: {e}")))?
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
                    continue; // binary
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
                "(no matches for \"{}\")",
                input.pattern
            )));
        }
        let mut body = lines.join("\n");
        if truncated {
            // US-026: report the truncation AND how to paginate (grep has no
            // offset -> we point toward narrowing the search).
            body.push_str(&format!(
                "\n[truncated: reached {MAX_MATCHES} matches; narrow with a more precise \
                 pattern, glob, or path to see the rest]"
            ));
        }
        Ok(ToolOutput::text(body))
    }
}

/// Truncates a display line to `max` BYTES on a UTF-8 character
/// boundary. Indispensable: `&line[..max]` panics when byte `max` falls in the middle
/// of a multi-byte codepoint (accented/CJK line > `max` bytes), which happens often on
/// source carrying accented text. We step back to the nearest boundary.
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
        // "a" + 150 x 'é' = 301 bytes: byte 300 falls in the MIDDLE of the 150th 'é'
        // -> `&line[..300]` would panic. The cut steps back to the boundary (299).
        let line = format!("a{}", "¢".repeat(150));
        assert!(line.len() > MAX_LINE_BYTES && !line.is_char_boundary(MAX_LINE_BYTES));
        let cut = truncate_line(&line, MAX_LINE_BYTES);
        assert!(cut.len() <= MAX_LINE_BYTES, "borné");
        assert!(
            line.starts_with(cut),
            "préfixe valide, aucune coupe mid-codepoint"
        );
    }
}
