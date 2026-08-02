use agent_core::auxiliary::realtime::{
    RealtimeCallSessionConfig, RealtimeConversationRole, RealtimeFramelessSessionConfig,
    RealtimeSessionConfig, RealtimeV1SessionConfig, RealtimeV2ConversationalSessionConfig,
    RealtimeV2SessionConfig, RealtimeV2TranscriptionSessionConfig,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};
use serde_json::{Value, json};

use super::super::validation::{nonempty, text};

const BACKGROUND_AGENT_TOOL_DESCRIPTION: &str = "Send a user request to the background agent. Use this as the default action. Do not rephrase the user's ask or rewrite it in your own words; pass along the user's own words. If the background agent is idle, this starts a new task and returns the final result to the user. If the background agent is already working on a task, this sends the request as guidance to steer that previous task. If the user asks to do something next, later, after this, or once current work finishes, call this tool so the work is actually queued instead of merely promising to do it later.";
const SILENCE_TOOL_DESCRIPTION: &str = "Call this when the best response is to say nothing. Use it instead of speaking after hidden system/control messages, after background agent updates in silent modes, or whenever acknowledging aloud would be distracting. This tool has no user-visible effect.";

pub(super) fn validate(config: &RealtimeSessionConfig) -> Result<(), AuxiliaryError> {
    let operation = AuxiliaryOperation::RealtimeWebSocket;
    match config {
        RealtimeSessionConfig::V1(config) => validate_v1(operation, config),
        RealtimeSessionConfig::FramelessBidi(config) => validate_frameless(operation, config),
        RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Conversational(config)) => {
            validate_v2_conversational(operation, config)
        }
        RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Transcription(config)) => {
            validate_v2_transcription(operation, config)
        }
    }
}

pub(super) fn validate_call(config: &RealtimeCallSessionConfig) -> Result<(), AuxiliaryError> {
    let operation = AuxiliaryOperation::RealtimeCall;
    match config {
        RealtimeCallSessionConfig::V1(config) => validate_v1(operation, config),
        RealtimeCallSessionConfig::FramelessBidi(config) => validate_frameless(operation, config),
    }
}

pub(super) fn update_value(config: &RealtimeSessionConfig) -> Value {
    let session = match config {
        RealtimeSessionConfig::V1(config) => v1_value(config),
        RealtimeSessionConfig::FramelessBidi(config) => frameless_value(config),
        RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Conversational(config)) => {
            v2_conversational_value(config)
        }
        RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Transcription(config)) => {
            v2_transcription_value(config)
        }
    };
    json!({"type": "session.update", "session": session})
}

pub(super) fn call_value(config: &RealtimeCallSessionConfig) -> Value {
    match config {
        RealtimeCallSessionConfig::V1(config) => with_model(v1_value(config), &config.model),
        RealtimeCallSessionConfig::FramelessBidi(config) => {
            with_model(frameless_value(config), &config.model)
        }
    }
}

fn validate_v1(
    operation: AuxiliaryOperation,
    config: &RealtimeV1SessionConfig,
) -> Result<(), AuxiliaryError> {
    text(operation, "instructions", &config.instructions, 1_000_000)?;
    validate_scope(
        operation,
        config.model.as_deref(),
        config.session_id.as_deref(),
    )
}

fn validate_frameless(
    operation: AuxiliaryOperation,
    config: &RealtimeFramelessSessionConfig,
) -> Result<(), AuxiliaryError> {
    text(operation, "instructions", &config.instructions, 1_000_000)?;
    validate_scope(
        operation,
        config.model.as_deref(),
        config.session_id.as_deref(),
    )?;
    for item in &config.initial_items {
        text(operation, "initial_items.text", &item.text, 1_000_000)?;
    }
    Ok(())
}

fn validate_v2_conversational(
    operation: AuxiliaryOperation,
    config: &RealtimeV2ConversationalSessionConfig,
) -> Result<(), AuxiliaryError> {
    text(operation, "instructions", &config.instructions, 1_000_000)?;
    validate_scope(
        operation,
        config.model.as_deref(),
        config.session_id.as_deref(),
    )
}

fn validate_v2_transcription(
    operation: AuxiliaryOperation,
    config: &RealtimeV2TranscriptionSessionConfig,
) -> Result<(), AuxiliaryError> {
    validate_scope(
        operation,
        config.model.as_deref(),
        config.session_id.as_deref(),
    )
}

fn validate_scope(
    operation: AuxiliaryOperation,
    model: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), AuxiliaryError> {
    if let Some(model) = model {
        nonempty(operation, "model", model, 256)?;
    }
    if let Some(session_id) = session_id {
        nonempty(operation, "session_id", session_id, 256)?;
    }
    Ok(())
}

fn v1_value(config: &RealtimeV1SessionConfig) -> Value {
    json!({
        "type": "quicksilver",
        "instructions": config.instructions,
        "audio": {
            "input": {"format": {"type": "audio/pcm", "rate": 24000}},
            "output": {"voice": config.voice},
        },
    })
}

fn frameless_value(config: &RealtimeFramelessSessionConfig) -> Value {
    let initial_items = config
        .initial_items
        .iter()
        .map(|item| {
            let content_type = match item.role {
                RealtimeConversationRole::Assistant => "output_text",
                RealtimeConversationRole::User | RealtimeConversationRole::Developer => {
                    "input_text"
                }
            };
            json!({
                "type": "message",
                "role": item.role,
                "content": [{"type": content_type, "text": item.text}],
            })
        })
        .collect::<Vec<_>>();
    let mut session = json!({
        "instructions": config.instructions,
        "audio": {"output": {"voice": config.voice}},
        "delegation": {"type": "client"},
    });
    if let Some(ack_filler) = config.delegation_ack_filler {
        session["delegation"]["ack_filler"] = Value::Bool(ack_filler);
    }
    if !initial_items.is_empty() {
        session["initial_items"] = Value::Array(initial_items);
    }
    session
}

fn v2_transcription_value(_config: &RealtimeV2TranscriptionSessionConfig) -> Value {
    json!({
        "type": "transcription",
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "transcription": {"model": "gpt-4o-mini-transcribe"},
            },
        },
    })
}

fn v2_conversational_value(config: &RealtimeV2ConversationalSessionConfig) -> Value {
    json!({
        "type": "realtime",
        "instructions": config.instructions,
        "output_modalities": [config.output_modality],
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "noise_reduction": {"type": "near_field"},
                "transcription": {"model": "gpt-4o-mini-transcribe"},
                "turn_detection": {
                    "type": "server_vad",
                    "interrupt_response": true,
                    "create_response": true,
                    "silence_duration_ms": 500,
                },
            },
            "output": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "voice": config.voice,
            },
        },
        "tools": [
            {
                "type": "function",
                "name": "background_agent",
                "description": BACKGROUND_AGENT_TOOL_DESCRIPTION,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The user request to delegate to the background agent."
                        }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "type": "function",
                "name": "remain_silent",
                "description": SILENCE_TOOL_DESCRIPTION,
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        ],
        "tool_choice": "auto",
    })
}

fn with_model(mut session: Value, model: &Option<String>) -> Value {
    if let Some(model) = model {
        session["model"] = Value::String(model.clone());
    }
    session
}

#[cfg(test)]
mod tests {
    use agent_core::auxiliary::realtime::{
        RealtimeOutputModality, RealtimeV2ConversationalSessionConfig, RealtimeVoice,
    };

    use super::*;

    #[test]
    fn call_and_update_shapes_have_distinct_model_placement() {
        let config = RealtimeV1SessionConfig {
            instructions: "Answer briefly.".into(),
            model: Some("gpt-realtime".into()),
            session_id: Some("session-1".into()),
            voice: RealtimeVoice::Cove,
        };
        let update = update_value(&RealtimeSessionConfig::V1(config.clone()));
        let call = call_value(&RealtimeCallSessionConfig::V1(config));
        assert!(update["session"].get("model").is_none());
        assert_eq!(call["model"], "gpt-realtime");
    }

    #[test]
    fn v2_conversational_shape_has_model_tools() {
        let config = RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Conversational(
            RealtimeV2ConversationalSessionConfig {
                instructions: "Answer briefly.".into(),
                model: Some("gpt-realtime".into()),
                session_id: None,
                output_modality: RealtimeOutputModality::Audio,
                voice: RealtimeVoice::Cove,
            },
        ));
        let update = update_value(&config);
        assert_eq!(update["session"]["type"], "realtime");
        assert_eq!(update["session"]["tools"][0]["name"], "background_agent");
        assert!(update["session"].get("model").is_none());
    }
}
