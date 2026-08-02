use agent_core::auxiliary::AuxiliaryError;
use agent_core::auxiliary::realtime::RealtimeEvent;
use serde_json::Value;

use super::{RealtimeCodec, audio_event, malformed, required_string, session_updated, unknown};

pub(super) fn parse(event_type: &str, value: &Value) -> Result<RealtimeEvent, AuxiliaryError> {
    let event = match event_type {
        "session.updated" => session_updated(value),
        "conversation.output_audio.delta" => RealtimeEvent::AudioOut(audio_event(value, false)?),
        "conversation.input_transcript.delta"
        | "conversation.item.input_audio_transcription.delta" => {
            RealtimeEvent::InputTranscriptDelta(required_string(value, "delta")?)
        }
        "conversation.input_transcript.turn_marked"
        | "conversation.item.input_audio_transcription.completed" => {
            RealtimeEvent::InputTranscriptDone(required_string(value, "transcript")?)
        }
        "conversation.output_transcript.delta"
        | "response.output_text.delta"
        | "response.output_audio_transcript.delta" => {
            RealtimeEvent::OutputTranscriptDelta(required_string(value, "delta")?)
        }
        "response.output_audio_transcript.done" => {
            RealtimeEvent::OutputTranscriptDone(required_string(value, "transcript")?)
        }
        "conversation.item.added" | "conversation.item.created" => {
            RealtimeEvent::ConversationItemAdded(value.get("item").cloned().ok_or_else(malformed)?)
        }
        "conversation.item.done" => RealtimeEvent::ConversationItemDone {
            item_id: required_string(value.get("item").ok_or_else(malformed)?, "id")?,
        },
        "conversation.handoff.requested" => RealtimeEvent::HandoffRequested {
            handoff_id: required_string(value, "handoff_id")?,
            item_id: required_string(value, "item_id")?,
            input_transcript: required_string(value, "input_transcript")?,
        },
        _ => return Err(unknown(RealtimeCodec::V1)),
    };
    Ok(event)
}
