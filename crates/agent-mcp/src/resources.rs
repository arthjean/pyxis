//! MCP resources, kept whole in one place.
//!
//! Resources are **not** exposed to the model. They are inspected on demand
//! through `/mcp <server> resources`, so a server cannot use them to push
//! content into a turn nobody asked for. That is also why the same treatment is
//! given to `initialize.instructions` (see `McpConnection::instructions`): both
//! are server-authored prose, and neither belongs in a tool description.

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
