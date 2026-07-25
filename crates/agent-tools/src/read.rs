//! `read` tool: reads a workspace file with line numbers. Read-only,
//! concurrency-safe, untrusted output (the content read can carry an injection,
//! OWASP LLM01). US-011 AC1/AC3.

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::error::ToolError;
use crate::path::{confine, ensure_existing_path_no_links};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Past this, the content is considered binary/unreadable (presence of NUL bytes
/// checked separately; this only bounds the size read in the MVP).
const MAX_BYTES: usize = 2_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
    /// Start line (1-indexed). Default: 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Max number of lines. Default: everything.
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
        "Read a workspace text file and return its contents prefixed with line \
         numbers. Parameters: path (relative to the workspace), offset \
         (1-indexed start line, optional), limit (line count, optional)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the workspace." },
                "offset": { "type": ["integer", "null"], "minimum": 1, "description": "Start line (1-indexed), or null." },
                "limit": { "type": ["integer", "null"], "minimum": 1, "description": "Maximum number of lines, or null." }
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
        ensure_existing_path_no_links(&ctx.workspace, &path, &input.path)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        let meta = file
            .metadata()
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        if meta.is_dir() {
            return Err(ToolError::Rejected(format!(
                "{} is a directory, not a file",
                input.path
            )));
        }
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        if bytes.contains(&0) {
            return Err(ToolError::Rejected(format!(
                "{} appears to be a binary file (NUL bytes)",
                input.path
            )));
        }
        // US-026: past MAX_BYTES, PARTIAL read (head of the file, cut on a
        // character boundary) + pagination hint, instead of a blunt rejection.
        let oversize = bytes.len() > MAX_BYTES;
        if oversize {
            bytes.truncate(MAX_BYTES);
        }
        let full = String::from_utf8_lossy(&bytes);
        let start = input.offset.unwrap_or(1).max(1);
        Ok(ToolOutput::text(render_read(
            full.as_ref(),
            start,
            input.limit,
            oversize,
        )))
    }
}

/// Renders the numbered lines of `text` from `start` (1-indexed), at most `limit`
/// lines, with continuation HINTS (US-026): limit reached ->
/// `[lines X-Y of Z; offset=Y+1 to continue]`; `oversize` -> partial read
/// hint; out-of-bounds range -> a hint rather than a vague message. Pure ->
/// testable without I/O.
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
            out.push_str("(empty file)");
        } else {
            out.push_str(&format!(
                "[range out of bounds: offset={start} > {total} lines]"
            ));
        }
        return out;
    }
    if oversize {
        out.push_str(&format!(
            "[file truncated at {MAX_BYTES} bytes ({emitted} lines read); read by \
             ranges with offset/limit]"
        ));
    } else if truncated_by_limit {
        out.push_str(&format!(
            "[lines {start}-{last_line} of {total}; offset={} to continue]",
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
            "full read should have no hint: {out}"
        );
    }

    #[test]
    fn limit_truncation_emits_continuation_hint() {
        let out = render_read(text5(), 1, Some(2), false);
        assert!(out.contains("     1\tl1"));
        assert!(out.contains("     2\tl2"));
        assert!(!out.contains("\tl3"));
        assert!(
            out.contains("[lines 1-2 of 5; offset=3 to continue]"),
            "pagination hint expected: {out}"
        );
    }

    #[test]
    fn out_of_range_offset_hints_instead_of_vague_message() {
        let out = render_read(text5(), 99, None, false);
        assert!(
            out.contains("[range out of bounds: offset=99 > 5 lines]"),
            "out-of-range hint expected: {out}"
        );
    }

    #[test]
    fn oversize_emits_partial_read_hint() {
        let out = render_read("a\nb\n", 1, None, true);
        assert!(out.contains("     1\ta"));
        assert!(
            out.contains("file truncated at") && out.contains("read by ranges"),
            "partial read hint expected: {out}"
        );
    }

    #[test]
    fn empty_file_reports_empty() {
        assert_eq!(render_read("", 1, None, false), "(empty file)");
    }
}
