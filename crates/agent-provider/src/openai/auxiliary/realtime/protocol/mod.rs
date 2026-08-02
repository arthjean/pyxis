mod frameless;
mod v1;
mod v2;

use agent_core::auxiliary::realtime::{
    RealtimeAudioFrame, RealtimeContextAppendChannel, RealtimeEvent, RealtimeSessionConfig,
    RealtimeWire,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase};
use serde_json::{Value, json};

use super::super::validation::text;

const MAX_REALTIME_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const FRAMELESS_CONTEXT_CHUNK_BYTES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RealtimeCodec {
    V1,
    FramelessBidi,
    RealtimeV2,
}

impl RealtimeCodec {
    pub(super) fn for_config(config: &RealtimeSessionConfig) -> Self {
        match config {
            RealtimeSessionConfig::V1(_) => Self::V1,
            RealtimeSessionConfig::FramelessBidi(_) => Self::FramelessBidi,
            RealtimeSessionConfig::RealtimeV2(_) => Self::RealtimeV2,
        }
    }

    pub(super) fn wire(self) -> RealtimeWire {
        match self {
            Self::V1 => RealtimeWire::V1,
            Self::FramelessBidi => RealtimeWire::FramelessBidi,
            Self::RealtimeV2 => RealtimeWire::RealtimeV2,
        }
    }

    pub(super) fn context_messages(
        self,
        context: &str,
        channel: Option<RealtimeContextAppendChannel>,
    ) -> Result<Vec<Value>, AuxiliaryError> {
        text(
            AuxiliaryOperation::RealtimeWebSocket,
            "context",
            context,
            MAX_REALTIME_MESSAGE_BYTES,
        )?;
        Ok(context_chunks(context, self)
            .into_iter()
            .map(|chunk| match self {
                Self::V1 | Self::RealtimeV2 => json!({
                    "type": "conversation.item.create",
                    "item": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": chunk}],
                    },
                }),
                Self::FramelessBidi => {
                    let mut message = json!({
                        "type": "session.context.append",
                        "content": [{"type": "input_text", "text": chunk}],
                    });
                    if let Some(channel) = channel {
                        message["channel"] = json!(channel);
                    }
                    message
                }
            })
            .collect())
    }

    pub(super) fn audio_message(self, frame: &RealtimeAudioFrame) -> Result<Value, AuxiliaryError> {
        if frame.data.is_empty() || frame.data.len() > MAX_REALTIME_MESSAGE_BYTES {
            return Err(AuxiliaryError::invalid(
                AuxiliaryOperation::RealtimeWebSocket,
                "audio",
                "audio frame is empty or exceeds 64 MiB",
            ));
        }
        let message_type = match self {
            Self::FramelessBidi => "input_audio.append",
            Self::V1 | Self::RealtimeV2 => "input_audio_buffer.append",
        };
        Ok(json!({"type": message_type, "audio": frame.data}))
    }

    pub(super) fn parse(self, payload: &str) -> Result<RealtimeEvent, AuxiliaryError> {
        let value: Value =
            serde_json::from_str(payload).map_err(|_| decode("malformed JSON frame"))?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| decode("frame has no type"))?;
        if event_type == "error" {
            return Ok(RealtimeEvent::Error(agent_core::redaction::redact_text(
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .filter(|message| message.len() <= 4096)
                    .unwrap_or("realtime provider error"),
            )));
        }
        match self {
            Self::V1 => v1::parse(event_type, &value),
            Self::FramelessBidi => frameless::parse(event_type, &value),
            Self::RealtimeV2 => v2::parse(event_type, &value),
        }
    }
}

pub(super) fn session_updated(value: &Value) -> RealtimeEvent {
    let session = value.get("session").unwrap_or(value);
    RealtimeEvent::SessionUpdated {
        realtime_session_id: string_at(session, "id"),
        instructions: string_at(session, "instructions"),
    }
}

pub(super) fn audio_event(
    value: &Value,
    default_geometry: bool,
) -> Result<RealtimeAudioFrame, AuxiliaryError> {
    Ok(RealtimeAudioFrame {
        data: string_at(value, "delta")
            .or_else(|| string_at(value, "data"))
            .ok_or_else(malformed)?,
        sample_rate: value
            .get("sample_rate")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .or(default_geometry.then_some(24_000))
            .ok_or_else(malformed)?,
        num_channels: value
            .get("channels")
            .or_else(|| value.get("num_channels"))
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .or(default_geometry.then_some(1))
            .ok_or_else(malformed)?,
        samples_per_channel: value
            .get("samples_per_channel")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        item_id: string_at(value, "item_id"),
    })
}

pub(super) fn response_id(value: &Value) -> Option<String> {
    value
        .get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| string_at(value, "response_id"))
}

pub(super) fn required_string(value: &Value, field: &str) -> Result<String, AuxiliaryError> {
    string_at(value, field).ok_or_else(malformed)
}

pub(super) fn string_at(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_REALTIME_MESSAGE_BYTES)
        .map(str::to_string)
}

pub(super) fn malformed() -> AuxiliaryError {
    decode("malformed realtime event")
}

pub(super) fn unknown(codec: RealtimeCodec) -> AuxiliaryError {
    decode(format!("unknown {codec:?} frame type"))
}

fn decode(reason: impl Into<String>) -> AuxiliaryError {
    AuxiliaryError::Decode {
        operation: AuxiliaryOperation::RealtimeWebSocket,
        phase: AuxiliaryPhase::Read,
        reason: reason.into(),
    }
}

fn context_chunks(text: &str, codec: RealtimeCodec) -> Vec<String> {
    if codec != RealtimeCodec::FramelessBidi || text.len() <= FRAMELESS_CONTEXT_CHUNK_BYTES {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + FRAMELESS_CONTEXT_CHUNK_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frameless_context_chunks_are_utf8_safe_and_bounded() {
        let text = "é".repeat(600);
        let chunks = context_chunks(&text, RealtimeCodec::FramelessBidi);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= FRAMELESS_CONTEXT_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn dialects_do_not_fallback_into_each_other() {
        assert!(RealtimeCodec::V1
            .parse(r#"{"type":"conversation.output_audio.delta","delta":"AA==","sample_rate":24000,"channels":1}"#)
            .is_ok());
        assert!(
            RealtimeCodec::FramelessBidi
                .parse(r#"{"type":"output_transcript.added","item":{"text":"hello"}}"#)
                .is_ok()
        );
        assert!(
            RealtimeCodec::RealtimeV2
                .parse(r#"{"type":"response.done","response":{"id":"resp_1"}}"#)
                .is_ok()
        );
        assert!(
            RealtimeCodec::V1
                .parse(r#"{"type":"response.done"}"#)
                .is_err()
        );
    }
}
