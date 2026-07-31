//! Calling a tool on a connected server (US-010): bounded wrapper around
//! `Peer::call_tool` and rendering of the result into the text the model will see.
//!
//! Two failure modes, kept apart as the protocol and the `rmcp` SDK impose:
//! a **functional** failure comes back as `Ok(CallToolResult { is_error: true })`
//! and is destined for the model; a **protocol or transport** failure comes back as
//! `Err(ServiceError)` and is destined for the harness. Only the second becomes an
//! `McpError`.

use std::time::Duration;

use agent_core::tools::ToolImage;
use agent_tools::image::{MAX_IMAGE_BYTES, sniff_base64_image};
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

/// Max images carried out of a single call. A browser server legitimately
/// returns one screenshot; a hostile one would return a thousand.
const MAX_IMAGES_PER_CALL: usize = 8;

/// Result of a call, as the tool layer consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutcome {
    /// Text handed to the model (already bounded).
    pub text: String,
    /// Protocol-native structure, retained whole for model-side rendering.
    pub structured_content: Option<serde_json::Value>,
    /// Functional failure reported by the server itself.
    pub is_error: bool,
    /// Images the call brought back, validated on their own bytes. Empty when
    /// the active model does not read images.
    pub images: Vec<ToolImage>,
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

    pub(crate) fn peer(&self) -> &Peer<RoleClient> {
        &self.peer
    }

    /// Invokes `tool` with `arguments`. Every error names the server.
    ///
    /// `vision` states whether the active model reads images. Fail-closed: with
    /// `false`, an image never leaves this function as anything but a descriptor.
    pub async fn call(
        &self,
        tool: &str,
        arguments: Option<JsonObject>,
        vision: bool,
    ) -> Result<McpCallOutcome, McpError> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let result = self.bounded_call(tool, self.peer.call_tool(params)).await?;
        Ok(render_result(&result, vision))
    }

    /// Runs one request under the call bound, mapping the SDK error taxonomy.
    /// `what` labels the failure: a tool name, or the protocol method for the
    /// requests that are not tool calls.
    pub(crate) async fn bounded_call<T>(
        &self,
        what: &str,
        work: impl std::future::Future<Output = Result<T, ServiceError>>,
    ) -> Result<T, McpError> {
        match tokio::time::timeout(self.timeout, work).await {
            Err(_elapsed) => Err(self.expired(what)),
            Ok(Err(err)) => Err(McpError::Call {
                server: self.server.clone(),
                tool: what.to_string(),
                message: call_error_message(&err),
            }),
            Ok(Ok(value)) => Ok(value),
        }
    }

    /// Same bound, for the listings whose failures are already rendered.
    pub(crate) async fn bounded<T>(
        &self,
        what: &str,
        work: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, McpError> {
        tokio::time::timeout(self.timeout, work)
            .await
            .map_err(|_| self.expired(what))?
            .map_err(|message| McpError::Call {
                server: self.server.clone(),
                tool: what.to_string(),
                message,
            })
    }

    fn expired(&self, what: &str) -> McpError {
        McpError::Call {
            server: self.server.clone(),
            tool: what.to_string(),
            message: format!("timeout after {}s", self.timeout.as_secs()),
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
/// reduced to a bounded descriptor, except an image the active model can read,
/// which is carried out separately as a content block (US-011).
fn render_result(result: &CallToolResult, vision: bool) -> McpCallOutcome {
    let mut images = Vec::new();
    let mut image_budget = MAX_IMAGE_BYTES;
    let mut parts: Vec<String> = result
        .content
        .iter()
        .map(|content| render_content(content, vision, &mut images, &mut image_budget))
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        let json = structured.to_string();
        if json.len() <= STRUCTURED_CAP {
            parts.push(json);
        } else {
            parts.push(format!(
                "[mcp structured content omitted: {} bytes]",
                json.len()
            ));
        }
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        text = "(no content)".to_string();
    }
    text = truncate_tail(&text, MAX_TOOL_OUTPUT_BYTES);
    McpCallOutcome {
        text,
        structured_content: result.structured_content.clone(),
        is_error: result.is_error.unwrap_or(false),
        images,
    }
}

/// Renders one content block, appending to `images` when the block is an image
/// the model can actually read and the budgets still allow it.
fn render_content(
    content: &Content,
    vision: bool,
    images: &mut Vec<ToolImage>,
    budget: &mut usize,
) -> String {
    match &content.raw {
        RawContent::Text(text) => text.text.clone(),
        RawContent::Image(image) => render_image(image, vision, images, budget),
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

/// Decides what happens to one image block. Every refusal produces a descriptor
/// naming its cause: the model must be able to tell "there was no image" from
/// "there was one and you cannot see it".
fn render_image(
    image: &rmcp::model::RawImageContent,
    vision: bool,
    images: &mut Vec<ToolImage>,
    budget: &mut usize,
) -> String {
    if !vision {
        return format!(
            "[mcp image omitted: the active model does not read images (mime={}, {} base64 bytes)]",
            image.mime_type,
            image.data.len()
        );
    }
    if images.len() >= MAX_IMAGES_PER_CALL {
        return format!(
            "[mcp image omitted: more than {MAX_IMAGES_PER_CALL} images in one result]"
        );
    }
    // The ANNOUNCED mime is not trusted: the decoded bytes decide, exactly like
    // `view_image` refuses a PDF named `.png`.
    match sniff_base64_image(&image.data, *budget) {
        Ok((media_type, bytes)) => {
            *budget = budget.saturating_sub(bytes);
            images.push(ToolImage {
                media_type: media_type.to_string(),
                data: image.data.clone(),
            });
            format!("[mcp image: {media_type}, {bytes} bytes, visible in the conversation]")
        }
        Err(reason) => format!("[mcp image omitted: {reason}]"),
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

    fn png_base64() -> String {
        use base64::Engine as _;
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(b"body");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn text_content_is_joined() {
        let out = render_result(&result(vec![text("a"), text("b")]), false);
        assert_eq!(out.text, "a\nb");
        assert!(!out.is_error);
    }

    #[test]
    fn functional_error_is_reported_as_error_outcome() {
        let out = render_result(&CallToolResult::error(vec![text("boom")]), false);
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }

    #[test]
    fn an_image_reaches_the_conversation_when_the_model_reads_images() {
        let out = render_result(
            &result(vec![Content::image(png_base64(), "image/png")]),
            true,
        );
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data, png_base64());
        // The text says the image exists without carrying its bytes.
        assert!(
            out.text.starts_with("[mcp image: image/png,"),
            "{}",
            out.text
        );
        assert!(!out.text.contains(&png_base64()));
    }

    #[test]
    fn without_vision_an_image_stays_a_descriptor() {
        let out = render_result(
            &result(vec![Content::image(png_base64(), "image/png")]),
            false,
        );
        assert!(out.images.is_empty());
        assert!(out.text.contains("does not read images"), "{}", out.text);
    }

    #[test]
    fn an_announced_mime_never_overrides_the_bytes() {
        use base64::Engine as _;
        let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7 not an image");
        let out = render_result(&result(vec![Content::image(pdf, "image/png")]), true);
        assert!(out.images.is_empty(), "a PDF must never be sent as a PNG");
        assert!(out.text.contains("not a PNG"), "{}", out.text);
    }

    #[test]
    fn the_image_count_is_bounded() {
        let blocks: Vec<Content> = (0..MAX_IMAGES_PER_CALL + 3)
            .map(|_| Content::image(png_base64(), "image/png"))
            .collect();
        let out = render_result(&result(blocks), true);
        assert_eq!(out.images.len(), MAX_IMAGES_PER_CALL);
        assert!(
            out.text.contains("more than 8 images"),
            "the excess must be named, not silently dropped: {}",
            out.text
        );
    }

    #[test]
    fn audio_content_becomes_a_bounded_descriptor() {
        let audio = Content::new(
            RawContent::Audio(rmcp::model::RawAudioContent {
                data: "A".repeat(4096),
                mime_type: "audio/wav".to_string(),
            }),
            None,
        );
        let out = render_result(&result(vec![audio]), true);
        assert_eq!(
            out.text,
            "[mcp audio omitted: mime=audio/wav, 4096 base64 bytes]"
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
        let out = render_result(&result(vec![resource]), false);
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
        let out = render_result(&result(vec![resource]), false);
        assert!(out.text.contains("file:///notes.md"));
        assert!(out.text.ends_with("hello"));
    }

    #[test]
    fn empty_result_falls_back_on_structured_then_placeholder() {
        let mut structured = result(Vec::new());
        structured.structured_content = Some(serde_json::json!({"ok": true}));
        let rendered = render_result(&structured, false);
        assert_eq!(rendered.text, r#"{"ok":true}"#);
        assert_eq!(
            rendered.structured_content,
            Some(serde_json::json!({"ok": true}))
        );
        assert_eq!(
            render_result(&result(Vec::new()), false).text,
            "(no content)"
        );
    }

    #[test]
    fn oversized_structured_only_result_uses_an_atomic_text_fallback() {
        let mut structured = result(Vec::new());
        structured.structured_content = Some(serde_json::json!({"blob": "x".repeat(20_000)}));

        let rendered = render_result(&structured, false);

        assert!(
            rendered
                .text
                .starts_with("[mcp structured content omitted:")
        );
        assert!(!rendered.text.contains("xxxx"));
        assert_eq!(rendered.structured_content, structured.structured_content);
    }

    #[test]
    fn oversized_text_is_tail_truncated() {
        let body = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 500);
        let out = render_result(&result(vec![text(&body)]), false);
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
