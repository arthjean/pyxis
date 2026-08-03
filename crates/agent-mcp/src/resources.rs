//! MCP resources, kept whole in one place.
//!
//! A server never PUSHES a resource: nothing here reaches a turn on its own,
//! and `initialize.instructions` gets the same treatment (see
//! `McpConnection::instructions`) because server-authored prose that lands in a
//! tool description is injection the taint defense structurally cannot see.
//!
//! What the model may do is PULL one, through the three tools in
//! `resource_tools` (US-012, ported from Codex
//! `core/src/tools/handlers/mcp_resource.rs`). The distinction is the whole
//! design: a pulled resource is an explicit call that goes through the same
//! pipeline as any MCP tool, so it is confirmed by default, tainted in full,
//! and visible in the transcript. A pushed one would be none of those things.
//!
//! Inspection by a human stays available and unchanged (`/mcp <server>
//! resources`), and never goes through the model at all.

use crate::call::McpClient;
use crate::error::McpError;
use crate::pagination::collect_paginated;
use crate::text::{DESCRIPTION_CAP, cap};

/// One resource a server advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// One parameterized resource URI a server advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

impl McpClient {
    /// Lists the resources the server advertises, with the same bounds as the
    /// tool listing.
    pub async fn list_resources(&self) -> Result<Vec<McpResourceInfo>, McpError> {
        let listing = collect_paginated("resources", |params| async move {
            let page = self
                .peer()
                .list_resources(params)
                .await
                .map_err(|e| e.to_string())?;
            Ok((page.resources, page.next_cursor))
        });
        let resources = self.bounded("resources/list", listing).await?;
        Ok(resources
            .into_iter()
            .map(|resource| McpResourceInfo {
                uri: resource.uri.clone(),
                name: resource.name.clone(),
                title: resource.title.clone(),
                description: resource
                    .description
                    .as_deref()
                    .map(|text| cap(text, DESCRIPTION_CAP)),
                mime_type: resource.mime_type.clone(),
            })
            .collect())
    }

    /// Lists the resource TEMPLATES the server advertises: parameterized URIs a
    /// caller fills in (`file:///{path}`). Bounded like the listing above.
    pub async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpError> {
        let listing = collect_paginated("resource templates", |params| async move {
            let page = self
                .peer()
                .list_resource_templates(params)
                .await
                .map_err(|e| e.to_string())?;
            Ok((page.resource_templates, page.next_cursor))
        });
        let templates = self.bounded("resources/templates/list", listing).await?;
        Ok(templates
            .into_iter()
            .map(|template| McpResourceTemplate {
                uri_template: template.uri_template.clone(),
                name: template.name.clone(),
                title: template.title.clone(),
                description: template
                    .description
                    .as_deref()
                    .map(|text| cap(text, DESCRIPTION_CAP)),
                mime_type: template.mime_type.clone(),
            })
            .collect())
    }

    /// Reads one resource. Text travels bounded; a binary blob never travels raw.
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let params = rmcp::model::ReadResourceRequestParams::new(uri.to_string());
        let result = self
            .bounded_call(uri, self.peer().read_resource(params))
            .await?;
        Ok(render_contents(&result.contents))
    }
}

/// Renders the contents of a resource read. Text travels (bounded by the shared
/// output cap); a blob is reduced to a descriptor, like any binary tool content.
fn render_contents(contents: &[rmcp::model::ResourceContents]) -> String {
    let rendered: Vec<String> = contents
        .iter()
        .map(|content| match content {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            rmcp::model::ResourceContents::BlobResourceContents { uri, blob, .. } => format!(
                "[mcp blob resource omitted: uri={uri}, {} base64 bytes]",
                blob.len()
            ),
        })
        .collect();
    let joined = rendered.join("\n");
    if joined.trim().is_empty() {
        "(empty resource)".to_string()
    } else {
        agent_tools::tool::truncate_tail(&joined, agent_tools::tool::MAX_TOOL_OUTPUT_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ResourceContents;

    #[test]
    fn text_contents_travel_and_blobs_become_descriptors() {
        let rendered = render_contents(&[
            ResourceContents::TextResourceContents {
                uri: "file:///a.md".to_string(),
                mime_type: None,
                text: "hello".to_string(),
                meta: None,
            },
            ResourceContents::BlobResourceContents {
                uri: "file:///b.bin".to_string(),
                mime_type: None,
                blob: "B".repeat(2048),
                meta: None,
            },
        ]);
        assert!(rendered.starts_with("hello"));
        assert!(rendered.contains("file:///b.bin"));
        assert!(rendered.contains("2048 base64 bytes"));
        assert!(!rendered.contains("BBBB"));
    }

    #[test]
    fn an_empty_read_is_named_rather_than_blank() {
        assert_eq!(render_contents(&[]), "(empty resource)");
    }
}
