//! `write` tool: creates or replaces a workspace file. Mutation confined
//! to the workspace (US-012 AC3; Landlock kernel enforcement in US-020). Its output is
//! a simple confirmation (not untrusted). US-012.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::path::{confine, ensure_not_protected, guard_protected_path, replace_file_confined};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_WRITE_BYTES, Tool, ToolCtx, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    pub path: String,
    pub content: String,
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    type Input = WriteInput;

    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> String {
        "Create or fully replace a workspace file. Parameters: path (relative \
         to the workspace), content (complete content). Missing parent \
         directories are created."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the workspace." },
                "content": { "type": "string", "description": "Complete content to write." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    // Mutation (not read-only), but a file edit (not "sensitive" in the
    // destructive/network sense) -> auto-accepted under AcceptEdits.
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// Output = in-house confirmation, not external content -> not untrusted.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        let bytes = input.content.len();
        if bytes > MAX_WRITE_BYTES {
            return Err(ValidationError::new(format!(
                "content too large: {bytes} bytes > {MAX_WRITE_BYTES}"
            )));
        }
        // US-013: refusal of deferred-execution zones, before the permission.
        guard_protected_path(&ctx.workspace, &input.path)
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        // Re-checked right before the write: between validation and here, a link
        // may have appeared in the workspace (US-013).
        ensure_not_protected(&ctx.workspace, &path, &input.path)?;
        let bytes = input.content.len();
        replace_file_confined(&ctx.workspace, &path, &input.path, input.content.as_bytes()).await?;
        Ok(ToolOutput::text(format!(
            "Wrote file: {} ({bytes} bytes)",
            input.path
        )))
    }
}
