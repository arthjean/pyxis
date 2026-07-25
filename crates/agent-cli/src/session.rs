//! `SharedSession`: wraps `JsonlSession` (US-009 persistence) while exposing an
//! **in-memory snapshot** of the transcript. Since the core calls
//! `sync(&messages)` with the FULL transcript on every turn
//! (transcript-before-response), the snapshot is always up to date: the interactive
//! loop reads it back to chain the turns without reimplementing the core's
//! message building.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_core::compaction::CompactKind;
use agent_core::message::{ContentBlock, Message, Role};
use agent_core::session::{FileSnapshot, Session, SessionError};
use agent_session::JsonlSession;
use async_trait::async_trait;

pub struct SharedSession {
    inner: JsonlSession,
    snapshot: Arc<Mutex<Vec<Message>>>,
}

impl SharedSession {
    /// Builds the shared session and also returns the snapshot handle that the
    /// interactive loop reads back between turns.
    pub fn new(inner: JsonlSession) -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                inner,
                snapshot: Arc::clone(&snapshot),
            }),
            snapshot,
        )
    }

    fn capture(&self, messages: &[Message]) {
        if let Ok(mut s) = self.snapshot.lock() {
            *s = messages.to_vec();
        }
    }

    fn redact_snapshot(&self) {
        if let Ok(mut s) = self.snapshot.lock() {
            for message in &mut *s {
                message
                    .content
                    .retain(|block| !matches!(block, ContentBlock::EncryptedReasoning { .. }));
            }
        }
    }

    /// Switches the persistence file to a resumed session (`/resume`).
    /// `cursor` = number of messages already present in the session (the next
    /// `sync` will only write what follows). The in-memory snapshot is updated separately
    /// by the interactive loop (`conversation` handle).
    pub fn switch_file(&self, path: &Path, cursor: usize) -> Result<(), SessionError> {
        self.inner.switch_to(path, cursor)
    }
}

fn strip_goal_done_marker(text: &mut String) -> bool {
    let trimmed = text.trim_end();
    let Some(last_line) = trimmed.lines().next_back() else {
        return false;
    };
    if last_line.trim() != crate::interactive::GOAL_DONE_MARKER {
        return false;
    }
    let marker_start = trimmed.len().saturating_sub(last_line.len());
    *text = trimmed[..marker_start].trim_end().to_string();
    true
}

fn sanitize_messages(messages: &[Message]) -> Vec<Message> {
    let mut sanitized = messages.to_vec();
    for message in sanitized.iter_mut().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        for block in message.content.iter_mut().rev() {
            if let ContentBlock::Text { text } = block
                && strip_goal_done_marker(text)
            {
                return sanitized;
            }
        }
        return sanitized;
    }
    sanitized
}

#[async_trait]
impl Session for SharedSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        let messages = sanitize_messages(messages);
        self.capture(&messages);
        self.inner.sync(&messages).await
    }

    async fn checkpoint(
        &self,
        kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        let messages = sanitize_messages(messages);
        self.capture(&messages);
        self.inner.checkpoint(kind, &messages).await
    }

    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
        self.inner.redact_encrypted_reasoning().await?;
        self.redact_snapshot();
        Ok(())
    }

    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError> {
        self.inner.record_file_snapshot(snapshot).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_messages_removes_final_goal_done_marker() {
        let messages = vec![
            Message::user("objectif"),
            Message::assistant_text(format!(
                "Terminé.\n{}",
                crate::interactive::GOAL_DONE_MARKER
            )),
        ];
        let sanitized = sanitize_messages(&messages);
        assert_eq!(sanitized[1].text(), "Terminé.");
    }

    #[test]
    fn sanitize_messages_keeps_inline_goal_done_marker() {
        let messages = vec![Message::assistant_text(format!(
            "Le marqueur {} est mentionné ici.",
            crate::interactive::GOAL_DONE_MARKER
        ))];
        let sanitized = sanitize_messages(&messages);
        assert_eq!(sanitized, messages);
    }
}
