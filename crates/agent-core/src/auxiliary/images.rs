use serde::{Deserialize, Serialize};

use crate::provider::ProviderExtension;

#[derive(Clone, PartialEq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub background: Option<ImageBackground>,
    pub model: String,
    pub n: Option<u64>,
    pub quality: Option<ImageQuality>,
    pub size: Option<String>,
    pub output_format: Option<String>,
}

impl std::fmt::Debug for ImageGenerationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageGenerationRequest")
            .field("prompt_bytes", &self.prompt.len())
            .field("model", &self.model)
            .field("background", &self.background)
            .field("n", &self.n)
            .field("quality", &self.quality)
            .field("size", &self.size)
            .field("output_format", &self.output_format)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct ImageEditRequest {
    pub images: Vec<ImageUrl>,
    pub mask: Option<ImageUrl>,
    pub prompt: String,
    pub background: Option<ImageBackground>,
    pub model: String,
    pub n: Option<u64>,
    pub quality: Option<ImageQuality>,
    pub size: Option<String>,
    pub output_format: Option<String>,
}

impl std::fmt::Debug for ImageEditRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageEditRequest")
            .field("image_count", &self.images.len())
            .field("has_mask", &self.mask.is_some())
            .field("prompt_bytes", &self.prompt.len())
            .field("model", &self.model)
            .field("background", &self.background)
            .field("n", &self.n)
            .field("quality", &self.quality)
            .field("size", &self.size)
            .field("output_format", &self.output_format)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImageUrl {
    pub image_url: String,
}

impl std::fmt::Debug for ImageUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageUrl")
            .field("image_url", &"[REDACTED_IMAGE_SOURCE]")
            .field("bytes", &self.image_url.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageBackground {
    Transparent,
    Opaque,
    Auto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageQuality {
    Low,
    Medium,
    High,
    Auto,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImageData {
    pub b64_json: Option<String>,
    pub url: Option<String>,
    pub revised_prompt: Option<String>,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageData")
            .field("has_b64_json", &self.b64_json.is_some())
            .field("has_url", &self.url.is_some())
            .field(
                "revised_prompt_bytes",
                &self.revised_prompt.as_ref().map(String::len),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct ImageResponse {
    pub created: u64,
    pub data: Vec<ImageData>,
    pub background: Option<ImageBackground>,
    pub quality: Option<ImageQuality>,
    pub size: Option<String>,
    pub output_format: Option<String>,
    pub metadata: ProviderExtension,
}

impl std::fmt::Debug for ImageResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageResponse")
            .field("created", &self.created)
            .field("data", &self.data)
            .field("background", &self.background)
            .field("quality", &self.quality)
            .field("size", &self.size)
            .field("output_format", &self.output_format)
            .field("metadata_redacted", &self.metadata.was_redacted())
            .finish()
    }
}
