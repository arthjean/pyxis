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
/// Files larger than this are not searched (most likely artifacts). Skipping is
/// REPORTED, never silent (US-078): a spilled 10 MiB output sits above this
/// bound, and answering "no matches" for a file that was never opened is a
/// false negative the model cannot tell from a real absence.
const MAX_FILE_BYTES: u64 = 5_000_000;
/// Skipped files named individually before the report aggregates. A directory
/// full of artifacts must cost a line or two, not one line per file: the report
/// exists to prevent a wrong conclusion, not to become the output.
const MAX_SKIP_REPORTS: usize = 5;
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

        let (lines, truncated, skipped) = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            let mut skipped: Vec<(String, u64)> = Vec::new();
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
                let rel = entry
                    .path()
                    .strip_prefix(&workspace)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE_BYTES {
                    skipped.push((rel, size));
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
            (out, truncated, skipped)
        })
        .await
        .map_err(|e| ToolError::Io(format!("walk: {e}")))?;

        let mut body = if lines.is_empty() {
            format!("(no matches for \"{}\")", input.pattern)
        } else {
            lines.join("\n")
        };
        if truncated {
            // US-026: report the truncation AND how to paginate (grep has no
            // offset -> we point toward narrowing the search).
            body.push_str(&format!(
                "\n[truncated: reached {MAX_MATCHES} matches; narrow with a more precise \
                 pattern, glob, or path to see the rest]"
            ));
        }
        // A search that skipped nothing produces exactly what it produced
        // before US-078, byte for byte.
        for line in skip_report(&skipped) {
            body.push('\n');
            body.push_str(&line);
        }
        Ok(ToolOutput::text(body))
    }
}

/// Turns the files left unsearched into a bounded report naming the only
/// recovery this batch actually made complete: `read` with `offset` and
/// `limit`, which pages a file of any size since US-077.
fn skip_report(skipped: &[(String, u64)]) -> Vec<String> {
    let mut out = Vec::new();
    for (rel, size) in skipped.iter().take(MAX_SKIP_REPORTS) {
        out.push(format!(
            "[skipped {rel}: {size} bytes, over the {MAX_FILE_BYTES}-byte search limit; \
             read it with `read` using offset and limit]"
        ));
    }
    if skipped.len() > MAX_SKIP_REPORTS {
        out.push(format!(
            "[skipped {} more files over the {MAX_FILE_BYTES}-byte search limit; read them \
             with `read` using offset and limit]",
            skipped.len() - MAX_SKIP_REPORTS
        ));
    }
    out
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
    fn no_skipped_file_produces_no_report_line() {
        assert!(skip_report(&[]).is_empty());
    }

    #[test]
    fn a_skipped_file_is_named_with_its_size_and_the_recovery() {
        let report = skip_report(&[("build.log".to_string(), 10_485_760)]);
        assert_eq!(report.len(), 1);
        assert!(report[0].contains("build.log"), "{}", report[0]);
        assert!(report[0].contains("10485760"), "{}", report[0]);
        assert!(
            report[0].contains("offset and limit"),
            "the recovery must be named: {}",
            report[0]
        );
    }

    #[test]
    fn many_skipped_files_collapse_into_one_aggregate_line() {
        let skipped: Vec<(String, u64)> = (0..12)
            .map(|i| (format!("artifact-{i}.log"), MAX_FILE_BYTES + 1))
            .collect();
        let report = skip_report(&skipped);
        assert_eq!(
            report.len(),
            MAX_SKIP_REPORTS + 1,
            "named files then one aggregate: {report:?}"
        );
        assert!(
            report[MAX_SKIP_REPORTS].contains(&format!("{} more files", 12 - MAX_SKIP_REPORTS)),
            "{}",
            report[MAX_SKIP_REPORTS]
        );
    }

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
