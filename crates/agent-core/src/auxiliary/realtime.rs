use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeWire {
    V1,
    FramelessBidi,
    RealtimeV2,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeOutputModality {
    Text,
    Audio,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeContextAppendChannel {
    Speakable,
    Commentary,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RealtimeConversationRole {
    User,
    Assistant,
    Developer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RealtimeConversationText {
    pub role: RealtimeConversationRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RealtimeVoice {
    Alloy,
    Arbor,
    Ash,
    Ballad,
    Breeze,
    Cedar,
    Coral,
    Cove,
    Echo,
    Ember,
    Juniper,
    Maple,
    Marin,
    Sage,
    Shimmer,
    Sol,
    Spruce,
    Vale,
    Verse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeV1SessionConfig {
    pub instructions: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub voice: RealtimeVoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeFramelessSessionConfig {
    pub instructions: String,
    pub initial_items: Vec<RealtimeConversationText>,
    pub delegation_ack_filler: Option<bool>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub voice: RealtimeVoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeV2ConversationalSessionConfig {
    pub instructions: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub output_modality: RealtimeOutputModality,
    pub voice: RealtimeVoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeV2TranscriptionSessionConfig {
    pub model: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeV2SessionConfig {
    Conversational(RealtimeV2ConversationalSessionConfig),
    Transcription(RealtimeV2TranscriptionSessionConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeSessionConfig {
    V1(RealtimeV1SessionConfig),
    FramelessBidi(RealtimeFramelessSessionConfig),
    RealtimeV2(RealtimeV2SessionConfig),
}

impl RealtimeSessionConfig {
    pub fn wire(&self) -> RealtimeWire {
        match self {
            Self::V1(_) => RealtimeWire::V1,
            Self::FramelessBidi(_) => RealtimeWire::FramelessBidi,
            Self::RealtimeV2(_) => RealtimeWire::RealtimeV2,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::V1(config) => config.model.as_deref(),
            Self::FramelessBidi(config) => config.model.as_deref(),
            Self::RealtimeV2(RealtimeV2SessionConfig::Conversational(config)) => {
                config.model.as_deref()
            }
            Self::RealtimeV2(RealtimeV2SessionConfig::Transcription(config)) => {
                config.model.as_deref()
            }
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::V1(config) => config.session_id.as_deref(),
            Self::FramelessBidi(config) => config.session_id.as_deref(),
            Self::RealtimeV2(RealtimeV2SessionConfig::Conversational(config)) => {
                config.session_id.as_deref()
            }
            Self::RealtimeV2(RealtimeV2SessionConfig::Transcription(config)) => {
                config.session_id.as_deref()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeCallSessionConfig {
    V1(RealtimeV1SessionConfig),
    FramelessBidi(RealtimeFramelessSessionConfig),
}

impl RealtimeCallSessionConfig {
    pub fn wire(&self) -> RealtimeWire {
        match self {
            Self::V1(_) => RealtimeWire::V1,
            Self::FramelessBidi(_) => RealtimeWire::FramelessBidi,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeCallRequest {
    pub sdp: String,
    pub session: RealtimeCallSessionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeCallResponse {
    pub sdp: String,
    pub call_id: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RealtimeAudioFrame {
    pub data: String,
    pub sample_rate: u32,
    pub num_channels: u16,
    pub samples_per_channel: Option<u32>,
    pub item_id: Option<String>,
}

impl std::fmt::Debug for RealtimeAudioFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RealtimeAudioFrame")
            .field("data", &"[REDACTED_AUDIO]")
            .field("encoded_bytes", &self.data.len())
            .field("sample_rate", &self.sample_rate)
            .field("num_channels", &self.num_channels)
            .field("samples_per_channel", &self.samples_per_channel)
            .field("item_id", &self.item_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RealtimeEvent {
    SessionUpdated {
        realtime_session_id: Option<String>,
        instructions: Option<String>,
    },
    InputTranscriptDelta(String),
    InputTranscriptDone(String),
    OutputTranscriptDelta(String),
    OutputTranscriptDone(String),
    AudioOut(RealtimeAudioFrame),
    ResponseCreated {
        response_id: Option<String>,
    },
    ResponseDone {
        response_id: Option<String>,
    },
    ResponseCancelled {
        response_id: Option<String>,
    },
    InputAudioSpeechStarted {
        item_id: Option<String>,
    },
    ConversationItemAdded(Value),
    ConversationItemDone {
        item_id: String,
    },
    HandoffRequested {
        handoff_id: String,
        item_id: String,
        input_transcript: String,
    },
    NoopRequested {
        call_id: String,
        item_id: String,
    },
    Error(String),
}
