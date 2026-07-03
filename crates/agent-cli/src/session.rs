//! `SharedSession` — enveloppe `JsonlSession` (persistance US-009) en exposant un
//! **snapshot en mémoire** du transcript. Comme le cœur appelle
//! `sync(&messages)` avec le transcript COMPLET à chaque tour
//! (transcript-before-response), le snapshot est toujours à jour : la boucle
//! interactive le relit pour enchaîner les tours sans réimplémenter la
//! construction de messages du cœur.

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
    /// Construit la session partagée et retourne aussi la poignée snapshot que la
    /// boucle interactive relit entre les tours.
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

    /// Bascule le fichier de persistance vers une session reprise (`/resume`).
    /// `cursor` = nombre de messages déjà présents dans la session (les prochains
    /// `sync` n'écriront que la suite). Le snapshot mémoire est mis à jour à part
    /// par la boucle interactive (poignée `conversation`).
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
