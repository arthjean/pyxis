//! Calling a tool on a connected server (US-010): bounded wrapper around
//! `Peer::call_tool` and rendering of the result into the text the model will see.
//!
//! Two failure modes, kept apart as the protocol and the `rmcp` SDK impose:
//! a **functional** failure comes back as `Ok(CallToolResult { is_error: true })`
//! and is destined for the model; a **protocol or transport** failure comes back as
//! `Err(ServiceError)` and is destined for the harness. Only the second becomes an
//! `McpError`.

use std::time::Duration;

use agent_tools::tool::{MAX_TOOL_OUTPUT_BYTES, truncate_tail};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, RawContent, ResourceContents,
};
use rmcp::{Peer, RoleClient, ServiceError};

use crate::error::McpError;

/// Default bound of one MCP call. `rmcp` 1.8 exposes no public per-request
/// timeout, so the bound is an envelope around the future: dropping it cancels
/// the request on our side and the connection stays usable.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap of a rendered structured payload, when it is the only content served.
const STRUCTURED_CAP: usize = 8_192;

/// Result of a call, as the tool layer consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutcome {
    /// Text handed to the model (already bounded).
    pub text: String,
    /// Functional failure reported by the server itself.
    pub is_error: bool,
}

/// Cloneable handle on a connected server, held by every tool that server exposes.
///
/// `Peer` is a channel to the service task, not the connection itself: cloning it
/// is cheap, and once the connection is closed (`cancel`, subprocess death) a call
/// fails with a transport error instead of hanging or panicking. The lifecycle
/// therefore stays owned by `McpRegistry` alone.
#[derive(Clone)]
pub struct McpClient {
    server: String,
    peer: Peer<RoleClient>,
    timeout: Duration,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.server)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    pub fn new(server: impl Into<String>, peer: Peer<RoleClient>) -> Self {
        Self {
            server: server.into(),
            peer,
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Invokes `tool` with `arguments`. Every error names the server.
    pub async fn call(
        &self,
        tool: &str,
        arguments: Option<JsonObject>,
    ) -> Result<McpCallOutcome, McpError> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        match tokio::time::timeout(self.timeout, self.peer.call_tool(params)).await {
            Err(_elapsed) => Err(McpError::Call {
                server: self.server.clone(),
                tool: tool.to_string(),
                message: format!("timeout after {}s", self.timeout.as_secs()),
            }),
            Ok(Err(err)) => Err(McpError::Call {
                server: self.server.clone(),
                tool: tool.to_string(),
                message: call_error_message(&err),
            }),
            Ok(Ok(result)) => Ok(render_result(&result)),
        }
    }
}

/// Message of a transport/protocol failure. A closed transport is the case where the
/// subprocess died in flight: it is named as such rather than left to the SDK
/// wording.
fn call_error_message(err: &ServiceError) -> String {
    match err {
        ServiceError::TransportClosed | ServiceError::TransportSend(_) => {
            "disconnected during the call".to_string()
        }
        other => other.to_string(),
    }
}

/// Renders a result as tool text. Non-textual content never travels raw: it is
/// reduced to a bounded descriptor.
fn render_result(result: &CallToolResult) -> McpCallOutcome {
    let mut parts: Vec<String> = result
        .content
        .iter()
        .map(render_content)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(truncate_tail(&structured.to_string(), STRUCTURED_CAP));
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        text = "(no content)".to_string();
    }
    text = truncate_tail(&text, MAX_TOOL_OUTPUT_BYTES);
    McpCallOutcome {
        text,
        is_error: result.is_error.unwrap_or(false),
    }
}

fn render_content(content: &Content) -> String {
    match &content.raw {
        RawContent::Text(text) => text.text.clone(),
        RawContent::Image(image) => format!(
            "[mcp image omitted: mime={}, {} base64 bytes]",
            image.mime_type,
            image.data.len()
        ),
        RawContent::Audio(audio) => format!(
            "[mcp audio omitted: mime={}, {} base64 bytes]",
            audio.mime_type,
            audio.data.len()
        ),
        RawContent::Resource(resource) => match &resource.resource {
            // A text resource IS text: it travels, bounded by the global cap.
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => format!("[mcp resource {uri}{}]\n{text}", mime_suffix(mime_type)),
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => format!(
                "[mcp blob resource omitted: uri={uri}{}, {} base64 bytes]",
                mime_suffix(mime_type),
                blob.len()
            ),
        },
        RawContent::ResourceLink(link) => format!(
            "[mcp resource link: uri={}, name={}{}]",
            link.uri,
            link.name,
            mime_suffix(&link.mime_type)
        ),
    }
}

fn mime_suffix(mime: &Option<String>) -> String {
    match mime {
        Some(mime) => format!(", mime={mime}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &str) -> Content {
        Content::text(body)
    }

    /// `is_error` absent: what a server that says nothing about it sends.
    fn result(content: Vec<Content>) -> CallToolResult {
        let mut result = CallToolResult::success(content);
        result.is_error = None;
        result
    }

    #[test]
    fn text_content_is_joined() {
        let out = render_result(&result(vec![text("a"), text("b")]));
        assert_eq!(out.text, "a\nb");
        assert!(!out.is_error);
    }

    #[test]
    fn functional_error_is_reported_as_error_outcome() {
        let out = render_result(&CallToolResult::error(vec![text("boom")]));
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }

    #[test]
    fn binary_content_becomes_a_bounded_descriptor() {
        let image = Content::image("A".repeat(4096), "image/png");
        let out = render_result(&result(vec![image]));
        assert_eq!(
            out.text,
            "[mcp image omitted: mime=image/png, 4096 base64 bytes]"
        );
        assert!(!out.text.contains("AAAA"));
    }

    #[test]
    fn blob_resource_never_travels_raw() {
        let resource = Content::resource(ResourceContents::BlobResourceContents {
            uri: "file:///x.bin".to_string(),
            mime_type: Some("application/octet-stream".to_string()),
            blob: "B".repeat(2048),
            meta: None,
        });
        let out = render_result(&result(vec![resource]));
        assert!(
            out.text
                .starts_with("[mcp blob resource omitted: uri=file:///x.bin")
        );
        assert!(out.text.contains("2048 base64 bytes"));
        assert!(!out.text.contains("BBBB"));
    }

    #[test]
    fn text_resource_travels_with_its_uri() {
        let resource = Content::embedded_text("file:///notes.md", "hello");
        let out = render_result(&result(vec![resource]));
        assert!(out.text.contains("file:///notes.md"));
        assert!(out.text.ends_with("hello"));
    }

    #[test]
    fn empty_result_falls_back_on_structured_then_placeholder() {
        let mut structured = result(Vec::new());
        structured.structured_content = Some(serde_json::json!({"ok": true}));
        assert_eq!(render_result(&structured).text, r#"{"ok":true}"#);
        assert_eq!(render_result(&result(Vec::new())).text, "(no content)");
    }

    #[test]
    fn oversized_text_is_tail_truncated() {
        let body = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 500);
        let out = render_result(&result(vec![text(&body)]));
        assert!(out.text.starts_with("[... output truncated,"));
        assert!(out.text.len() < body.len());
    }

    #[test]
    fn closed_transport_is_named_as_a_disconnection() {
        assert_eq!(
            call_error_message(&ServiceError::TransportClosed),
            "disconnected during the call"
        );
        assert!(
            call_error_message(&ServiceError::Timeout {
                timeout: Duration::from_secs(1)
            })
            .contains("timeout")
        );
    }
}
