//! Contrat de persistance de session (injecté). L'implémentation JSONL
//! append-only + resume est `agent-session` (US-009) ; le cœur ne connaît que
//! ce trait et les types d'entrée canoniques.

use serde::{Deserialize, Serialize};

use crate::compaction::CompactKind;
use crate::message::Message;

/// Entrée de log discriminée (ARCHITECTURE §7). Sérialisée une par ligne JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum SessionEntry {
    Message(Message),
    CompactBoundary { kind: CompactKind },
    FileHistorySnapshot(FileSnapshot),
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
    /// Persiste les messages pas encore écrits (transcript-before-response,
    /// invariant 6). DOIT être idempotent : n'écrit que le delta depuis le
    /// dernier `sync` (l'implémentation tient un curseur).
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError>;

    /// Checkpoint de compaction **full** (auto/reactive) : écrit la frontière
    /// `CompactBoundary` ET le transcript post-compaction de façon **atomique**
    /// (même opération), puis resynchronise le curseur sur `messages.len()`.
    /// Évite une frontière orpheline (un crash entre la frontière et le résumé
    /// rendrait un transcript vide au resume). La microcompaction, elle, est
    /// purement en mémoire et n'appelle PAS ceci.
    async fn checkpoint(&self, kind: CompactKind, messages: &[Message])
    -> Result<(), SessionError>;

    /// Écrit un snapshot de fichier (entrée discriminée `FileHistorySnapshot`).
    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError>;
}
