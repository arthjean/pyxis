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
    /// `None` under `--ephemeral` (US-020 AC4): the snapshot still feeds the
    /// turn chaining, but nothing reaches the disk. Modelled as an absent writer
    /// rather than a writer pointed at a throwaway file, because the guarantee
    /// asked for is that no file was written at all, not that one was cleaned up.
    inner: Option<JsonlSession>,
    snapshot: Arc<Mutex<Vec<Message>>>,
}

impl SharedSession {
    /// Builds the shared session and also returns the snapshot handle that the
    /// interactive loop reads back between turns.
    pub fn new(inner: JsonlSession) -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        Self::build(Some(inner))
    }

    /// Session that persists nothing (`--ephemeral`, headless only).
    pub fn ephemeral() -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        Self::build(None)
    }

    fn build(inner: Option<JsonlSession>) -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
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
        match &self.inner {
            Some(inner) => inner.switch_to(path, cursor),
            None => Err(SessionError::Io(
                "this session is ephemeral: it persists nothing".into(),
            )),
        }
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
        match &self.inner {
            Some(inner) => inner.sync(&messages).await,
            None => Ok(()),
        }
    }

    async fn checkpoint(
        &self,
        kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        let messages = sanitize_messages(messages);
        self.capture(&messages);
        match &self.inner {
            Some(inner) => inner.checkpoint(kind, &messages).await,
            None => Ok(()),
        }
    }

    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
        if let Some(inner) = &self.inner {
            inner.redact_encrypted_reasoning().await?;
        }
        self.redact_snapshot();
        Ok(())
    }

    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError> {
        match &self.inner {
            Some(inner) => inner.record_file_snapshot(snapshot).await,
            None => Ok(()),
        }
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

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis-session-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// US-019 AC3: the fork mechanism. `switch_file(new, 0)` resets the cursor, so
    /// the next `sync` replays the WHOLE transcript into the fresh file, and the
    /// original one is closed with exactly the bytes it already had.
    #[tokio::test]
    async fn forking_copies_the_transcript_and_leaves_the_original_intact() {
        let dir = tmp_dir("fork");
        let origin = dir.join("origin.jsonl");
        let fork = dir.join("fork.jsonl");
        let messages = vec![Message::user("bonjour"), Message::assistant_text("salut")];

        let (session, _snapshot) = SharedSession::new(JsonlSession::create_at(&origin).unwrap());
        session.sync(&messages).await.unwrap();
        let origin_before = std::fs::read_to_string(&origin).unwrap();

        session.switch_file(&fork, 0).unwrap();
        session.sync(&messages).await.unwrap();
        // One more turn: it lands in the fork alone.
        let mut grown = messages.clone();
        grown.push(Message::user("suite"));
        session.sync(&grown).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&origin).unwrap(),
            origin_before,
            "l'originale ne bouge plus après le fork"
        );
        let resumed = agent_session::resume_file(&fork).unwrap();
        assert_eq!(resumed.messages, grown, "la copie part de l'état courant");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// US-020 AC4: an ephemeral session keeps feeding the turn chaining through
    /// its snapshot, but touches no file at all.
    #[tokio::test]
    async fn an_ephemeral_session_writes_nothing() {
        let dir = tmp_dir("ephemeral");
        let (session, snapshot) = SharedSession::ephemeral();
        let messages = vec![Message::user("bonjour"), Message::assistant_text("salut")];

        session.sync(&messages).await.unwrap();
        session
            .checkpoint(CompactKind::Auto, &messages)
            .await
            .unwrap();
        session.redact_encrypted_reasoning().await.unwrap();

        assert_eq!(
            snapshot.lock().unwrap().len(),
            2,
            "le snapshot vit toujours"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "aucun fichier écrit"
        );
        // Moving the persistence file of a session that has none is an error, not
        // a silent no-op: `/fork` would otherwise claim a copy that does not exist.
        assert!(session.switch_file(&dir.join("x.jsonl"), 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
