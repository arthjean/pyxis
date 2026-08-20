//! `glob` tool: lists the workspace files matching a glob pattern
//! (`**/*.rs`, `src/*.toml`, ...). Read-only, concurrency-safe. US-011 AC2.

use async_trait::async_trait;
use globset::Glob as GlobPattern;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::error::{ToolError, ValidationError};
use crate::path::{confine, ensure_existing_path_no_links, is_in_protected_subpath};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Result cap (avoids flooding the prompt on a huge repo).
const MAX_MATCHES: usize = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    pub pattern: String,
    /// Base subdirectory (relative to the workspace). Default: workspace root.
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
        "List workspace files matching a glob pattern (for example \"**/*.rs\" \
         or \"src/*.toml\"). Parameters: pattern (the glob), path (optional base \
         subdirectory). Returned paths are relative to the workspace."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, for example **/*.rs." },
                "path": { "type": ["string", "null"], "description": "Base subdirectory relative to the workspace, or null." }
            },
            "required": ["pattern", "path"],
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
        GlobPattern::new(&input.pattern)
            .map(|_| ())
            .map_err(|e| ValidationError::new(format!("invalid glob pattern: {e}")))
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let matcher = GlobPattern::new(&input.pattern)
            .map_err(|e| ToolError::Rejected(format!("invalid glob pattern: {e}")))?
            .compile_matcher();
        let base = match &input.path {
            Some(p) => confine(&ctx.workspace, p)?,
            None => ctx.workspace.clone(),
        };
        ensure_existing_path_no_links(&ctx.workspace, &base, input.path.as_deref().unwrap_or("."))?;
        let workspace = ctx.workspace.clone();
        let pattern = input.pattern.clone();

        // US-079: the walk stays out of the protected subpaths (`.pyxis`, hence
        // the spill root and the session files; `.git` internals too), unless
        // the base explicitly names one, which keeps a spilled artifact
        // listable when it is asked for by path.
        let prune_protected = !is_in_protected_subpath(&workspace, &base);

        // Synchronous walk (blocking FS) moved off the async runtime.
        let matches = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            for entry in WalkDir::new(&base)
                .into_iter()
                .filter_entry(|e| {
                    !prune_protected || !is_in_protected_subpath(&workspace, e.path())
                })
                .flatten()
            {
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
            return Ok(ToolOutput::text(format!("(no files match \"{pattern}\")")));
        }
        let mut body = matches.join("\n");
        if matches.len() >= MAX_MATCHES {
            body.push_str(&format!("\n... (truncated at {MAX_MATCHES} results)"));
        }
        Ok(ToolOutput::text(body))
    }
}
