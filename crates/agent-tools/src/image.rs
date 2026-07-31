//! `view_image` tool (US-011): brings a workspace image into the model's
//! context. Ported from Codex
//! (`codex-rs/core/src/tools/handlers/view_image_spec.rs:15-40`): same single
//! `path` parameter, same purpose.
//!
//! The image does not travel in the `tool_result` block, which carries text
//! only: the tool hands it to the pipeline through `ToolOutput::images` and the
//! loop turns it into a `ContentBlock::Image` in a user message right after the
//! result. Full compaction already strips those blocks (we do not pay for
//! vision twice), which is exactly AC5.
//!
//! The MEDIA TYPE comes from the file's magic bytes, never from its extension:
//! a `.png` holding a PDF must be refused, not announced as an image to the
//! provider.

use agent_core::tools::ToolImage;
use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::error::{ToolError, ValidationError};
use crate::path::{confine, ensure_existing_path_no_links};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Size cap of an image, BEFORE base64 (which inflates by ~4/3). Past this,
/// the payload costs more than what a screenshot is worth.
pub const MAX_IMAGE_BYTES: usize = 5_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewImageInput {
    pub path: String,
}

pub struct ViewImage;

#[async_trait]
impl Tool for ViewImage {
    type Input = ViewImageInput;

    fn name(&self) -> &str {
        "view_image"
    }
    fn description(&self) -> String {
        format!(
            "Read a workspace image (PNG, JPEG, GIF, WebP) and add it to the \
             conversation so the model can look at it. Only for what text \
             cannot express: a screenshot, a diagram, a rendering. Maximum \
             {MAX_IMAGE_BYTES} bytes. Parameter: path (relative to the workspace)."
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Image path relative to the workspace." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    /// Reads a file and grows the context: harmless to run beside other reads.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// The image is external content: untrusted like any file read (OWASP
    /// LLM01). A diagram can carry an instruction addressed to the model.
    fn returns_untrusted(&self) -> bool {
        true
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    /// AC2: without vision declared by the provider, the call is refused HERE,
    /// hence before the permission decision and before a single byte is read.
    /// Fail-closed: an unknown capability reads as absent.
    fn validate_input(&self, _input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        if !ctx.vision {
            return Err(ValidationError::new(
                "the active model does not read images: view_image is unusable \
                 in this session (describe the file in text instead)",
            ));
        }
        Ok(())
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // AC3: outside the read perimeter -> refused by the shared guardrail,
        // with its dedicated error.
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
                "{} is a directory, not an image",
                input.path
            )));
        }
        // AC4: the size is checked BEFORE reading, so an oversized file never
        // becomes an allocation.
        if meta.len() > MAX_IMAGE_BYTES as u64 {
            return Err(ToolError::Rejected(format!(
                "{} is too large: {} bytes > {MAX_IMAGE_BYTES}",
                input.path,
                meta.len()
            )));
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        file.take(MAX_IMAGE_BYTES as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;

        let Some(media_type) = sniff_media_type(&bytes) else {
            return Err(ToolError::Rejected(format!(
                "{} is not a supported image: expected PNG, JPEG, GIF or WebP \
                 (decided on the file's magic bytes, not its extension)",
                input.path
            )));
        };
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(ToolOutput::text(format!(
            "Image read: {} ({}, {} bytes). It is now visible in the conversation.",
            input.path,
            media_type,
            bytes.len()
        ))
        .with_images(vec![ToolImage {
            media_type: media_type.to_string(),
            data,
        }]))
    }
}

/// Validates a base64 payload a third party announced as an image (an MCP
/// server's `image` content). Returns the media type and the decoded size.
///
/// The announced mime type is deliberately ignored: it is a claim by the party
/// that also supplies the bytes. The rule of `view_image` applies unchanged, the
/// magic bytes decide, so a provider is never handed something it was told is a
/// PNG and is not.
///
/// `Err` carries a reason short enough to become a descriptor in the tool text.
pub fn sniff_base64_image(data: &str, max_bytes: usize) -> Result<(&'static str, usize), String> {
    // Rejected on the estimate first: an oversized payload never becomes an
    // allocation. Base64 encodes 3 bytes per 4 chars.
    let estimated = data.len() / 4 * 3;
    if estimated > max_bytes {
        return Err(format!("too large: ~{estimated} bytes > {max_bytes}"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "invalid base64".to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!("too large: {} bytes > {max_bytes}", bytes.len()));
    }
    match sniff_media_type(&bytes) {
        Some(media_type) => Ok((media_type, bytes.len())),
        None => Err("not a PNG, JPEG, GIF or WebP".to_string()),
    }
}

/// Media type decided by the file's magic bytes. `None` = not an image we know
/// how to announce, so we refuse rather than let the provider guess.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP: the size sits between the two markers, hence the two tests.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(b"body");
        v
    }

    #[test]
    fn magic_bytes_decide_the_media_type() {
        assert_eq!(sniff_media_type(&png_bytes()), Some("image/png"));
        assert_eq!(
            sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_media_type(b"GIF89a....."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_media_type(&webp), Some("image/webp"));
    }

    #[test]
    fn a_non_image_is_not_announced_as_one() {
        assert_eq!(sniff_media_type(b"%PDF-1.7"), None);
        assert_eq!(sniff_media_type(b""), None);
        assert_eq!(sniff_media_type(b"RIFF____AVI "), None);
    }

    #[test]
    fn a_third_party_payload_is_decided_by_its_bytes() {
        let png = base64::engine::general_purpose::STANDARD.encode(png_bytes());
        assert_eq!(
            sniff_base64_image(&png, MAX_IMAGE_BYTES),
            Ok(("image/png", png_bytes().len()))
        );
        // A PDF announced as an image is refused, whatever the server claims.
        let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7 nope");
        assert!(
            sniff_base64_image(&pdf, MAX_IMAGE_BYTES)
                .unwrap_err()
                .contains("not a PNG")
        );
        assert!(
            sniff_base64_image("!!not base64!!", MAX_IMAGE_BYTES)
                .unwrap_err()
                .contains("invalid base64")
        );
    }

    #[test]
    fn an_oversized_payload_is_refused_on_the_estimate() {
        // 4 chars per 3 bytes: this never gets decoded.
        let huge = "A".repeat(64);
        let err = sniff_base64_image(&huge, 8).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn without_vision_the_call_is_refused_before_any_read() {
        let dir = std::env::temp_dir().join(format!("pyxis-view-image-{}", std::process::id()));
        let ctx = ToolCtx::new(&dir);
        let err = ViewImage
            .validate_input(
                &ViewImageInput {
                    // The file does not even exist: the refusal must not depend on it.
                    path: "absent.png".to_string(),
                },
                &ctx,
            )
            .expect_err("without vision the call must be refused");
        assert!(err.to_string().contains("does not read images"), "{err}");
    }

    #[tokio::test]
    async fn a_declared_image_is_read_and_carried_as_a_block() {
        let dir = std::env::temp_dir().join(format!(
            "pyxis-view-image-ok-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("shot.png"), png_bytes())
            .await
            .unwrap();
        let ctx = ToolCtx::new(&dir).with_vision(true);
        let out = ViewImage
            .call(
                ViewImageInput {
                    path: "shot.png".to_string(),
                },
                &ctx,
            )
            .await
            .expect("a valid PNG must be read");
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert!(!out.images[0].data.is_empty());
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_file_that_is_not_an_image_names_the_cause() {
        let dir = std::env::temp_dir().join(format!(
            "pyxis-view-image-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("fake.png"), b"%PDF-1.7 not an image")
            .await
            .unwrap();
        let ctx = ToolCtx::new(&dir).with_vision(true);
        let err = ViewImage
            .call(
                ViewImageInput {
                    path: "fake.png".to_string(),
                },
                &ctx,
            )
            .await
            .expect_err("a PDF disguised as a PNG must be refused");
        assert!(err.to_string().contains("not a supported image"), "{err}");
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
