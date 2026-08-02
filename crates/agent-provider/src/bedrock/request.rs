//! Canonical request conversion for Amazon Bedrock Converse.

use std::collections::HashMap;

use agent_core::message::{ContentBlock, Role, ToolCallFormat};
use agent_core::provider::{ProviderError, ToolKind};
use aws_sdk_bedrockruntime::types::{
    ConversationRole, JsonSchemaDefinition, Message, OutputConfig, OutputFormat,
    OutputFormatStructure, OutputFormatType, Tool, ToolConfiguration, ToolInputSchema,
    ToolResultBlock, ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Blob, Document, Number};
use base64::Engine;

use super::invalid;

pub(super) fn message_to_bedrock(
    message: &agent_core::message::Message,
) -> Result<Message, ProviderError> {
    let role = match message.role {
        Role::Assistant => ConversationRole::Assistant,
        Role::User | Role::Tool => ConversationRole::User,
        Role::System => {
            return Err(invalid(
                "messages",
                "system messages must use the system field",
            ));
        }
    };
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } | ContentBlock::Summary { text, .. } => {
                content.push(aws_sdk_bedrockruntime::types::ContentBlock::Text(
                    text.clone(),
                ));
            }
            ContentBlock::Thinking { .. } | ContentBlock::EncryptedReasoning { .. } => {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "reasoning_history".into(),
                    reason: "Bedrock reasoning history requires provider-specific signatures"
                        .into(),
                });
            }
            ContentBlock::Image { media_type, data } => {
                content.push(aws_sdk_bedrockruntime::types::ContentBlock::Image(
                    image_block(media_type, data)?,
                ));
            }
            ContentBlock::ToolUse {
                id,
                name,
                input,
                format,
            } => {
                if *format != ToolCallFormat::Json {
                    return Err(ProviderError::UnsupportedTool {
                        tool: name.clone(),
                        reason: "Bedrock tool inputs must be JSON".into(),
                    });
                }
                let tool = ToolUseBlock::builder()
                    .tool_use_id(id)
                    .name(name)
                    .input(json_to_document(input)?)
                    .build()
                    .map_err(|_| invalid("messages", "invalid tool-use block"))?;
                content.push(aws_sdk_bedrockruntime::types::ContentBlock::ToolUse(tool));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content: text,
                structured_content,
                is_error,
                images,
                ..
            } => {
                let mut builder = ToolResultBlock::builder()
                    .tool_use_id(tool_use_id)
                    .content(ToolResultContentBlock::Text(text.clone()))
                    .status(if *is_error {
                        ToolResultStatus::Error
                    } else {
                        ToolResultStatus::Success
                    });
                if let Some(structured) = structured_content {
                    builder = builder
                        .content(ToolResultContentBlock::Json(json_to_document(structured)?));
                }
                for image in images {
                    builder = builder.content(ToolResultContentBlock::Image(image_block(
                        &image.media_type,
                        &image.data,
                    )?));
                }
                let result = builder
                    .build()
                    .map_err(|_| invalid("messages", "invalid tool-result block"))?;
                content.push(aws_sdk_bedrockruntime::types::ContentBlock::ToolResult(
                    result,
                ));
            }
        }
    }
    Message::builder()
        .role(role)
        .set_content(Some(content))
        .build()
        .map_err(|_| invalid("messages", "message content must not be empty"))
}

fn image_block(
    media_type: &str,
    encoded: &str,
) -> Result<aws_sdk_bedrockruntime::types::ImageBlock, ProviderError> {
    let format = match media_type.to_ascii_lowercase().as_str() {
        "image/gif" => aws_sdk_bedrockruntime::types::ImageFormat::Gif,
        "image/jpeg" | "image/jpg" => aws_sdk_bedrockruntime::types::ImageFormat::Jpeg,
        "image/png" => aws_sdk_bedrockruntime::types::ImageFormat::Png,
        "image/webp" => aws_sdk_bedrockruntime::types::ImageFormat::Webp,
        _ => return Err(invalid("media_type", "unsupported Bedrock image format")),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid("image_data", "invalid base64 image"))?;
    aws_sdk_bedrockruntime::types::ImageBlock::builder()
        .format(format)
        .source(aws_sdk_bedrockruntime::types::ImageSource::Bytes(
            Blob::new(bytes),
        ))
        .build()
        .map_err(|_| invalid("image_data", "invalid Bedrock image"))
}

pub(super) fn build_tool_configuration(
    tools: &[agent_core::provider::ToolSpec],
) -> Result<ToolConfiguration, ProviderError> {
    let mut builder = ToolConfiguration::builder();
    for tool in tools {
        let ToolKind::Function {
            input_schema,
            strict,
            defer_loading,
            output_schema,
        } = &tool.kind
        else {
            return Err(ProviderError::UnsupportedTool {
                tool: tool.name.clone(),
                reason: "Bedrock Converse accepts function tools only".into(),
            });
        };
        if *defer_loading || output_schema.is_some() {
            return Err(ProviderError::UnsupportedTool {
                tool: tool.name.clone(),
                reason: "Bedrock Converse does not encode deferred or output tool schemas".into(),
            });
        }
        let specification = ToolSpecification::builder()
            .name(&tool.name)
            .description(&tool.description)
            .input_schema(ToolInputSchema::Json(json_to_document(input_schema)?))
            .strict(*strict)
            .build()
            .map_err(|_| invalid("tools", format!("invalid tool `{}`", tool.name)))?;
        builder = builder.tools(Tool::ToolSpec(specification));
    }
    builder
        .build()
        .map_err(|_| invalid("tools", "tool configuration must not be empty"))
}

pub(super) fn build_output_config(
    output: &agent_core::provider::OutputSchema,
) -> Result<OutputConfig, ProviderError> {
    let schema = serde_json::to_string(&output.schema)
        .map_err(|_| invalid("output_schema", "schema serialization failed"))?;
    let definition = JsonSchemaDefinition::builder()
        .schema(schema)
        .name(&output.name)
        .build()
        .map_err(|_| invalid("output_schema", "invalid Bedrock JSON schema definition"))?;
    let format = OutputFormat::builder()
        .r#type(OutputFormatType::JsonSchema)
        .structure(OutputFormatStructure::JsonSchema(definition))
        .build()
        .map_err(|_| invalid("output_schema", "invalid Bedrock output format"))?;
    Ok(OutputConfig::builder().text_format(format).build())
}

fn json_to_document(value: &serde_json::Value) -> Result<Document, ProviderError> {
    Ok(match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(value) => Document::Bool(*value),
        serde_json::Value::String(value) => Document::String(value.clone()),
        serde_json::Value::Array(values) => Document::Array(
            values
                .iter()
                .map(json_to_document)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_json::Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_to_document(value)?)))
                .collect::<Result<HashMap<_, _>, ProviderError>>()?,
        ),
        serde_json::Value::Number(value) => {
            let number = if let Some(value) = value.as_u64() {
                Number::PosInt(value)
            } else if let Some(value) = value.as_i64() {
                Number::NegInt(value)
            } else {
                Number::Float(
                    value
                        .as_f64()
                        .ok_or_else(|| invalid("json", "non-finite number"))?,
                )
            };
            Document::Number(number)
        }
    })
}
