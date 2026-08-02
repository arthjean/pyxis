//! Provider-neutral contracts for non-sampling provider operations.

pub mod compact;
pub mod files;
pub mod images;
pub mod realtime;
pub mod search;

use crate::provider::AuthError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxiliaryOperation {
    RemoteCompact,
    Memories,
    ImageGeneration,
    ImageEdit,
    Search,
    FileUpload,
    RealtimeCall,
    RealtimeWebSocket,
}

impl std::fmt::Display for AuxiliaryOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RemoteCompact => "remote_compact",
            Self::Memories => "memories",
            Self::ImageGeneration => "image_generation",
            Self::ImageEdit => "image_edit",
            Self::Search => "search",
            Self::FileUpload => "file_upload",
            Self::RealtimeCall => "realtime_call",
            Self::RealtimeWebSocket => "realtime_websocket",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxiliaryPhase {
    Validate,
    Request,
    Decode,
    BlobUpload,
    Finalize,
    Connect,
    Write,
    Read,
    Close,
}

impl std::fmt::Display for AuxiliaryPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Validate => "validate",
            Self::Request => "request",
            Self::Decode => "decode",
            Self::BlobUpload => "blob_upload",
            Self::Finalize => "finalize",
            Self::Connect => "connect",
            Self::Write => "write",
            Self::Read => "read",
            Self::Close => "close",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuxiliaryError {
    #[error("{operation}: provider capability is unsupported")]
    Unsupported { operation: AuxiliaryOperation },
    #[error("{operation} {phase}: invalid field `{field}`: {reason}")]
    InvalidInput {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        field: &'static str,
        reason: String,
    },
    #[error("{operation} {phase}: operation timed out")]
    Timeout {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
    },
    #[error("{operation} {phase}: operation cancelled")]
    Cancelled {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
    },
    #[error("{operation} {phase}: transport failed ({kind})")]
    Transport {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        kind: &'static str,
    },
    #[error(
        "file_upload blob_upload: transport failed ({kind}, azure_client_request_id={azure_client_request_id})"
    )]
    BlobUploadTransport {
        kind: &'static str,
        azure_client_request_id: String,
    },
    #[error("{operation} {phase}: authentication failed ({error:?})")]
    Auth {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        error: AuthError,
    },
    #[error("{operation} {phase}: HTTP {status}")]
    Http {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        status: u16,
        request_id: Option<String>,
        azure_client_request_id: Option<String>,
        azure_request_id: Option<String>,
        azure_error_code: Option<String>,
    },
    #[error("{operation} {phase}: malformed provider payload ({reason})")]
    Decode {
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        reason: String,
    },
    #[error(
        "file_upload validate: file `{file_name}` is {size_bytes} bytes, limit is {limit_bytes}"
    )]
    FileTooLarge {
        file_name: String,
        size_bytes: u64,
        limit_bytes: u64,
    },
    #[error("file_upload validate: declared size {declared_bytes} differs from {actual_bytes}")]
    FileSizeMismatch {
        declared_bytes: u64,
        actual_bytes: u64,
    },
    #[error("file_upload finalize: upload `{file_id}` is not ready")]
    UploadNotReady { file_id: String },
    #[error("file_upload finalize: upload `{file_id}` failed ({reason})")]
    UploadFailed { file_id: String, reason: String },
}

impl AuxiliaryError {
    pub fn invalid(
        operation: AuxiliaryOperation,
        field: &'static str,
        reason: impl Into<String>,
    ) -> Self {
        Self::InvalidInput {
            operation,
            phase: AuxiliaryPhase::Validate,
            field,
            reason: reason.into(),
        }
    }

    pub fn decode(operation: AuxiliaryOperation, reason: impl Into<String>) -> Self {
        Self::Decode {
            operation,
            phase: AuxiliaryPhase::Decode,
            reason: reason.into(),
        }
    }
}

#[async_trait::async_trait]
pub trait RealtimeSession: Send + Sync {
    async fn append_context(&self, text: &str) -> Result<(), AuxiliaryError>;

    async fn append_context_with_channel(
        &self,
        text: &str,
        channel: Option<realtime::RealtimeContextAppendChannel>,
    ) -> Result<(), AuxiliaryError>;

    async fn send_audio(&self, frame: &realtime::RealtimeAudioFrame) -> Result<(), AuxiliaryError>;

    async fn next_event(&self) -> Result<Option<realtime::RealtimeEvent>, AuxiliaryError>;

    async fn close(&self) -> Result<(), AuxiliaryError>;
}

#[async_trait::async_trait]
pub trait AuxiliaryProvider: Send + Sync {
    async fn compact_remote(
        &self,
        request: &compact::CompactRequest,
    ) -> Result<compact::CompactResponse, AuxiliaryError>;

    async fn summarize_memories(
        &self,
        request: &compact::MemorySummarizeInput,
    ) -> Result<Vec<compact::MemorySummarizeOutput>, AuxiliaryError>;

    async fn generate_image(
        &self,
        request: &images::ImageGenerationRequest,
    ) -> Result<images::ImageResponse, AuxiliaryError>;

    async fn edit_image(
        &self,
        request: &images::ImageEditRequest,
    ) -> Result<images::ImageResponse, AuxiliaryError>;

    async fn search(
        &self,
        request: &search::SearchRequest,
    ) -> Result<search::SearchResponse, AuxiliaryError>;

    async fn upload_file(
        &self,
        request: files::FileUploadRequest,
    ) -> Result<files::UploadedFile, AuxiliaryError>;

    async fn create_realtime_call(
        &self,
        request: &realtime::RealtimeCallRequest,
    ) -> Result<realtime::RealtimeCallResponse, AuxiliaryError>;

    async fn connect_realtime(
        &self,
        config: realtime::RealtimeSessionConfig,
    ) -> Result<Box<dyn RealtimeSession>, AuxiliaryError>;

    async fn connect_realtime_sideband(
        &self,
        config: realtime::RealtimeSessionConfig,
        call_id: &str,
    ) -> Result<Box<dyn RealtimeSession>, AuxiliaryError>;
}
