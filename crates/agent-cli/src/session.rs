//! `SharedSession` — enveloppe `JsonlSession` (persistance US-009) en exposant un
//! **snapshot en mémoire** du transcript. Comme le cœur appelle
//! `sync(&messages)` avec le transcript COMPLET à chaque tour
//! (transcript-before-response), le snapshot est toujours à jour : la boucle
//! interactive le relit pour enchaîner les tours sans réimplémenter la
//! construction de messages du cœur.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_core::compaction::CompactKind;
use agent_core::message::Message;
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

    /// Bascule le fichier de persistance vers une session reprise (`/resume`).
    /// `cursor` = nombre de messages déjà présents dans la session (les prochains
    /// `sync` n'écriront que la suite). Le snapshot mémoire est mis à jour à part
    /// par la boucle interactive (poignée `conversation`).
    pub fn switch_file(&self, path: &Path, cursor: usize) -> Result<(), SessionError> {
        self.inner.switch_to(path, cursor)
    }
}

#[async_trait]
impl Session for SharedSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        self.capture(messages);
        self.inner.sync(messages).await
    }

    async fn checkpoint(
        &self,
        kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        self.capture(messages);
        self.inner.checkpoint(kind, messages).await
    }

    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError> {
        self.inner.record_file_snapshot(snapshot).await
    }
}
