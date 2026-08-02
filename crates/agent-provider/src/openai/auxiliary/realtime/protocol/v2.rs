use agent_core::auxiliary::AuxiliaryError;
use agent_core::auxiliary::realtime::RealtimeEvent;
use serde_json::Value;

use super::{
    RealtimeCodec, audio_event, malformed, required_string, response_id, session_updated,
    string_at, unknown,
};

pub(super) fn parse(event_type: &str, value: &Value) -> Result<RealtimeEvent, AuxiliaryError> {
    let event = match event_type {
        "session.updated" => session_updated(value),
        "response.output_audio.delta" | "response.audio.delta" => {
            RealtimeEvent::AudioOut(audio_event(value, true)?)
        }
        "conversation.item.input_audio_transcription.delta" => {
            RealtimeEvent::InputTranscriptDelta(required_string(value, "delta")?)
        }
        "conversation.item.input_audio_transcription.completed" => {
            RealtimeEvent::InputTranscriptDone(required_string(value, "transcript")?)
        }
        "response.output_text.delta" | "response.output_audio_transcript.delta" => {
            RealtimeEvent::OutputTranscriptDelta(required_string(value, "delta")?)
        }
        "response.output_audio_transcript.done" => {
            RealtimeEvent::OutputTranscriptDone(required_string(value, "transcript")?)
        }
        "response.output_text.done" => {
            RealtimeEvent::OutputTranscriptDone(required_string(value, "text")?)
        }
        "response.created" => RealtimeEvent::ResponseCreated {
            response_id: response_id(value),
        },
        "response.done" => RealtimeEvent::ResponseDone {
            response_id: response_id(value),
        },
        "response.cancelled" => RealtimeEvent::ResponseCancelled {
            response_id: response_id(value),
        },
        "input_audio_buffer.speech_started" => RealtimeEvent::InputAudioSpeechStarted {
            item_id: string_at(value, "item_id"),
        },
        "conversation.item.added" | "conversation.item.created" => {
            RealtimeEvent::ConversationItemAdded(value.get("item").cloned().ok_or_else(malformed)?)
        }
        "conversation.item.done" => parse_done_item(value)?,
        _ => return Err(unknown(RealtimeCodec::RealtimeV2)),
    };
    Ok(event)
}

fn parse_done_item(value: &Value) -> Result<RealtimeEvent, AuxiliaryError> {
    let item = value.get("item").ok_or_else(malformed)?;
    if item.get("type").and_then(Value::as_str) == Some("function_call")
        && item.get("name").and_then(Value::as_str) == Some("background_agent")
    {
        let handoff_id = string_at(item, "call_id")
            .or_else(|| string_at(item, "id"))
            .ok_or_else(malformed)?;
        let item_id = string_at(item, "id").unwrap_or_else(|| handoff_id.clone());
        let arguments = string_at(item, "arguments").unwrap_or_default();
        let input_transcript = extract_input_transcript(arguments);
        return Ok(RealtimeEvent::HandoffRequested {
            handoff_id,
            item_id,
            input_transcript,
        });
    }
    if item.get("type").and_then(Value::as_str) == Some("function_call")
        && item.get("name").and_then(Value::as_str) == Some("remain_silent")
    {
        let call_id = string_at(item, "call_id")
            .or_else(|| string_at(item, "id"))
            .ok_or_else(malformed)?;
        let item_id = string_at(item, "id").unwrap_or_else(|| call_id.clone());
        return Ok(RealtimeEvent::NoopRequested { call_id, item_id });
    }
    Ok(RealtimeEvent::ConversationItemDone {
        item_id: required_string(item, "id")?,
    })
}

fn extract_input_transcript(arguments: String) -> String {
    serde_json::from_str::<Value>(&arguments)
        .ok()
        .and_then(|arguments| {
            ["input_transcript", "input", "text", "prompt", "query"]
                .into_iter()
                .find_map(|key| string_at(&arguments, key))
        })
        .unwrap_or(arguments)
}
