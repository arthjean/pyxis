//! Session persistence contract (injected). The append-only JSONL + resume
//! implementation is `agent-session` (US-009); the core only knows this trait
//! and the canonical entry types.

use serde::{Deserialize, Serialize};

use crate::compaction::CompactKind;
use crate::message::Message;

/// Discriminated log entry (ARCHITECTURE section 7). Serialized one per JSONL line.
/// `CompactBoundary` stays readable for older logs; new compaction
/// checkpoints go through `CompactCheckpoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum SessionEntry {
    Meta {
        schema_version: u32,
    },
    Message(Message),
    CompactBoundary {
        kind: CompactKind,
    },
    CompactCheckpoint {
        kind: CompactKind,
        messages: Vec<Message>,
    },
    EncryptedReasoningRedacted,
    FileHistorySnapshot(FileSnapshot),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: String,
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(String),
    #[error("serde: {0}")]
    Serde(String),
}

#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Persists the messages not yet written (transcript-before-response,
    /// invariant 6). MUST be idempotent: it only writes the delta since the
    /// last `sync` (the implementation holds a cursor).
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError>;

    /// **full** compaction checkpoint (auto/reactive): writes the post-compaction
    /// transcript as a single replayable entry, then resynchronizes the cursor on
    /// `messages.len()`. Microcompaction, in contrast, is purely in memory and
    /// does NOT call this.
    async fn checkpoint(&self, kind: CompactKind, messages: &[Message])
    -> Result<(), SessionError>;

    /// Records a durable redaction of the already persisted encrypted reasoning
    /// blocks. Replay applies that redaction to the rebuilt messages.
    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError>;

    /// Writes a file snapshot (discriminated `FileHistorySnapshot` entry).
    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError>;
}
