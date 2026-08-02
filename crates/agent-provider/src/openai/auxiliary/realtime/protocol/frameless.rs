use agent_core::auxiliary::AuxiliaryError;
use agent_core::auxiliary::realtime::{RealtimeAudioFrame, RealtimeEvent};
use serde_json::Value;

use super::{RealtimeCodec, malformed, required_string, session_updated, unknown};

pub(super) fn parse(event_type: &str, value: &Value) -> Result<RealtimeEvent, AuxiliaryError> {
    let event = match event_type {
        "session.started" => session_updated(value),
        "output_audio.delta" => RealtimeEvent::AudioOut(RealtimeAudioFrame {
            data: required_string(value, "audio")?,
            sample_rate: 24_000,
            num_channels: 1,
            samples_per_channel: None,
            item_id: None,
        }),
        "input_transcript.added" => RealtimeEvent::InputTranscriptDelta(required_item_text(value)?),
        "output_transcript.added" => {
            RealtimeEvent::OutputTranscriptDelta(required_item_text(value)?)
        }
        "turn.done" => {
            let turn = value.get("turn").ok_or_else(malformed)?;
            let text = required_string(turn, "transcript")?;
            match turn.get("role").and_then(Value::as_str) {
                Some("user") => RealtimeEvent::InputTranscriptDone(text),
                Some("assistant") => RealtimeEvent::OutputTranscriptDone(text),
                _ => return Err(malformed()),
            }
        }
        "delegation.created" => parse_delegation(value)?,
        _ => return Err(unknown(RealtimeCodec::FramelessBidi)),
    };
    Ok(event)
}

fn required_item_text(value: &Value) -> Result<String, AuxiliaryError> {
    value
        .get("item")
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(malformed)
}

fn parse_delegation(value: &Value) -> Result<RealtimeEvent, AuxiliaryError> {
    let item = value.get("item").ok_or_else(malformed)?;
    if item.get("type").and_then(Value::as_str) != Some("delegation")
        || item.get("target").and_then(Value::as_str) != Some("client")
    {
        return Err(malformed());
    }
    let item_id = required_string(item, "id")?;
    let input_transcript = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    Ok(RealtimeEvent::HandoffRequested {
        handoff_id: item_id.clone(),
        item_id,
        input_transcript,
    })
}
