use agent_core::auxiliary::images::{
    ImageBackground, ImageData, ImageEditRequest, ImageGenerationRequest, ImageQuality,
    ImageResponse, ImageUrl,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};
use serde::Serialize;
use serde_json::Value;

use super::ConfiguredOpenAiProvider;
use super::json::{decode, optional_string, optional_typed, sanitized_metadata};
use super::validation::{nonempty, text};

#[derive(Serialize)]
struct ImageGenerationWireRequest<'a> {
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<ImageBackground>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<ImageQuality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_format: Option<&'a str>,
}

#[derive(Serialize)]
struct ImageEditWireRequest<'a> {
    images: Vec<ImageUrlWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mask: Option<ImageUrlWire<'a>>,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<ImageBackground>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<ImageQuality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_format: Option<&'a str>,
}

#[derive(Serialize)]
struct ImageUrlWire<'a> {
    image_url: &'a str,
}

impl<'a> From<&'a ImageGenerationRequest> for ImageGenerationWireRequest<'a> {
    fn from(request: &'a ImageGenerationRequest) -> Self {
        Self {
            prompt: &request.prompt,
            background: request.background,
            model: &request.model,
            n: request.n,
            quality: request.quality,
            size: request.size.as_deref(),
            output_format: request.output_format.as_deref(),
        }
    }
}

pub(super) async fn generate(
    provider: &ConfiguredOpenAiProvider,
    request: &ImageGenerationRequest,
) -> Result<ImageResponse, AuxiliaryError> {
    let operation = AuxiliaryOperation::ImageGeneration;
    super::ensure_supported(provider, operation)?;
    validate_request(operation, &request.model, &request.prompt, request.n)?;
    let response = provider
        .auxiliary_json(
            operation,
            "images/generations",
            &ImageGenerationWireRequest::from(request),
        )
        .await?;
    decode_response(operation, &response.body)
}

pub(super) async fn edit(
    provider: &ConfiguredOpenAiProvider,
    request: &ImageEditRequest,
) -> Result<ImageResponse, AuxiliaryError> {
    let operation = AuxiliaryOperation::ImageEdit;
    super::ensure_supported(provider, operation)?;
    validate_request(operation, &request.model, &request.prompt, request.n)?;
    if request.images.is_empty() || request.images.len() > 16 {
        return Err(AuxiliaryError::invalid(
            operation,
            "images",
            "expected 1..=16 image sources",
        ));
    }
    for image in request.images.iter().chain(request.mask.iter()) {
        validate_image_url(operation, image)?;
    }
    let wire = ImageEditWireRequest {
        images: request
            .images
            .iter()
            .map(|image| ImageUrlWire {
                image_url: &image.image_url,
            })
            .collect(),
        mask: request.mask.as_ref().map(|image| ImageUrlWire {
            image_url: &image.image_url,
        }),
        prompt: &request.prompt,
        background: request.background,
        model: &request.model,
        n: request.n,
        quality: request.quality,
        size: request.size.as_deref(),
        output_format: request.output_format.as_deref(),
    };
    let response = provider
        .auxiliary_json(operation, "images/edits", &wire)
        .await?;
    decode_response(operation, &response.body)
}

fn validate_request(
    operation: AuxiliaryOperation,
    model: &str,
    prompt: &str,
    n: Option<u64>,
) -> Result<(), AuxiliaryError> {
    nonempty(operation, "model", model, 256)?;
    text(operation, "prompt", prompt, 1_000_000)?;
    if n.is_some_and(|n| !(1..=10).contains(&n)) {
        return Err(AuxiliaryError::invalid(operation, "n", "expected 1..=10"));
    }
    Ok(())
}

fn validate_image_url(
    operation: AuxiliaryOperation,
    image: &ImageUrl,
) -> Result<(), AuxiliaryError> {
    nonempty(operation, "image_url", &image.image_url, 64 * 1024 * 1024)
}

fn decode_response(
    operation: AuxiliaryOperation,
    body: &[u8],
) -> Result<ImageResponse, AuxiliaryError> {
    let value: Value = decode(operation, body)?;
    let created = value
        .get("created")
        .and_then(Value::as_u64)
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing created timestamp"))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .filter(|data| !data.is_empty())
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing or empty image data array"))?;
    let data = data
        .iter()
        .map(|item| {
            let b64_json = optional_string(operation, item.get("b64_json"), 64 * 1024 * 1024)?;
            let url = optional_string(operation, item.get("url"), 64 * 1024)?;
            if b64_json.is_none() && url.is_none() {
                return Err(AuxiliaryError::decode(
                    operation,
                    "image item has neither data nor URL",
                ));
            }
            Ok(ImageData {
                b64_json,
                url,
                revised_prompt: optional_string(operation, item.get("revised_prompt"), 1_000_000)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = sanitized_metadata("auxiliary.image.response", value.clone(), &["data"]);
    Ok(ImageResponse {
        created,
        data,
        background: optional_typed(&value, "background", operation)?,
        quality: optional_typed(&value, "quality", operation)?,
        size: optional_string(operation, value.get("size"), 128)?,
        output_format: optional_string(operation, value.get("output_format"), 128)?,
        metadata,
    })
}
