//! `SharedSession`: an **in-memory snapshot** of the transcript layered over the
//! durable writer. Since the core calls `sync(&messages)` with the FULL
//! transcript on every turn (transcript-before-response), the snapshot is always
//! up to date: the runtime reads it back to chain the turns without
//! reimplementing the core's message building.
//!
//! The writer underneath is the thread log itself (`JsonlThreadStore`, which
//! implements `Session`). Since EP-005 a conversation has ONE durable file and
//! ONE handle on it: switching session no longer moves a file under a live
//! writer, it opens another thread runtime.

use std::sync::{Arc, Mutex};

use agent_core::compaction::CompactKind;
use agent_core::message::{ContentBlock, Message, Role};
use agent_core::session::{FileSnapshot, Session, SessionError};
use agent_core::{ContextBaseline, ContextTransition};
use async_trait::async_trait;

pub struct SharedSession {
    /// `None` under `--ephemeral` (US-018 AC4): the snapshot still feeds the
    /// turn chaining, but nothing reaches the disk. Modelled as an absent writer
    /// rather than a writer pointed at a throwaway file, because the guarantee
    /// asked for is that no file was written at all, not that one was cleaned up.
    inner: Option<Arc<dyn Session>>,
    snapshot: Arc<Mutex<Vec<Message>>>,
    baseline: Mutex<Option<ContextBaseline>>,
}

impl SharedSession {
    /// Builds the shared session over a durable writer and returns the snapshot
    /// handle the runtime reads back between turns.
    pub fn over(inner: Arc<dyn Session>) -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        Self::build(Some(inner))
    }

    /// Session that persists nothing (`--ephemeral`, headless only).
    pub fn ephemeral() -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        Self::build(None)
    }

    fn build(inner: Option<Arc<dyn Session>>) -> (Arc<Self>, Arc<Mutex<Vec<Message>>>) {
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        let baseline = inner
            .as_ref()
            .and_then(|session| session.context_baseline());
        (
            Arc::new(Self {
                inner,
                snapshot: Arc::clone(&snapshot),
                baseline: Mutex::new(baseline),
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
    fn context_baseline(&self) -> Option<ContextBaseline> {
        self.baseline
            .lock()
            .ok()
            .and_then(|baseline| baseline.clone())
    }

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
        if let Some(inner) = &self.inner {
            inner.checkpoint(kind, &messages).await?;
        }
        if let Ok(mut baseline) = self.baseline.lock() {
            *baseline = None;
        }
        Ok(())
    }

    async fn record_context_transition(
        &self,
        transition: ContextTransition,
    ) -> Result<(), SessionError> {
        if let Some(inner) = &self.inner {
            inner.record_context_transition(transition.clone()).await?;
        }
        if let Ok(mut baseline) = self.baseline.lock() {
            *baseline = Some(transition.to);
        }
        Ok(())
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

    /// The snapshot layer writes THROUGH to the durable log and mirrors what it
    /// wrote, which is what lets the runtime chain a turn on the transcript
    /// without re-reading the file.
    #[tokio::test]
    async fn the_snapshot_mirrors_what_the_durable_writer_took() {
        let dir = tmp_dir("over");
        let path = dir.join("thread.jsonl");
        let store = Arc::new(agent_session::JsonlThreadStore::open(&path).unwrap());
        let (session, snapshot) = SharedSession::over(Arc::clone(&store) as Arc<dyn Session>);

        let messages = vec![Message::user("bonjour"), Message::assistant_text("salut")];
        session.sync(&messages).await.unwrap();

        assert_eq!(snapshot.lock().unwrap().len(), 2);
        let resumed = agent_session::resume_file(&path).unwrap();
        assert_eq!(resumed.messages, messages);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// US-018 AC4: an ephemeral session keeps feeding the turn chaining through
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
