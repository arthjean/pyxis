//! JSONL adapter of `ThreadStore` (US-003).
//!
//! Writes the orchestration events into the SAME file as the v1 transcript, as
//! additive discriminated entries. That is what makes a v1 session continuable
//! without rewriting its prefix, and what will let US-010 materialize a fork by
//! copying one durable prefix instead of stitching two files.
//!
//! Durability is the one of `agent-session`: `write_all` + `flush` +
//! `sync_data` per line, so an accepted append survived before the caller is
//! acknowledged. A failed write poisons the writer: ordering can no longer be
//! promised, so everything is refused until the thread is reopened.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agent_core::session::{SessionEntry, SessionError};
use agent_runtime::event::{THREAD_RUNTIME_VERSION, ThreadEvent};
use agent_runtime::id::ThreadId;
use agent_runtime::store::{StoreError, ThreadSnapshot, ThreadStore};
use serde::Serialize;

use crate::{
    SESSION_SCHEMA_VERSION, ThreadLine, WriterState, open_prepared, resume_file, write_buf_locked,
};

fn map_session_error(err: SessionError) -> StoreError {
    match err {
        SessionError::Io(detail) => StoreError::Io(detail),
        SessionError::Serde(detail) => StoreError::Serde(detail),
        SessionError::Corrupt { offset, detail } => StoreError::Corrupt { offset, detail },
    }
}

pub struct JsonlThreadStore {
    path: PathBuf,
    state: Mutex<StoreState>,
}

struct StoreState {
    /// `None` once closed: the file lock is released with the handle.
    writer: Option<WriterState>,
    /// Thread this log is bound to, learned at open or set by `create`.
    thread_id: Option<ThreadId>,
}

fn write_json<T: Serialize>(writer: &mut WriterState, value: &T) -> Result<(), StoreError> {
    if writer.is_poisoned() {
        return Err(StoreError::Poisoned);
    }
    let line = serde_json::to_string(value).map_err(|e| StoreError::Serde(e.to_string()))?;
    match write_buf_locked(writer, &format!("{line}\n")) {
        Ok(()) => Ok(()),
        Err(err) if writer.is_poisoned() => {
            // Keep the underlying cause visible: `Poisoned` alone says the
            // writer is unusable, not why the append failed.
            tracing_detail(&err);
            Err(StoreError::Poisoned)
        }
        Err(err) => Err(map_session_error(err)),
    }
}

fn tracing_detail(err: &SessionError) {
    tracing::warn!(error = %err, "thread store append failed; writer poisoned");
}

impl JsonlThreadStore {
    /// Opens (creating when needed) the thread log at `path` and takes its
    /// exclusive lock. A partial trailing line is truncated at the last valid
    /// offset, as for any session file.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let (mut writer, is_empty, resumed) = open_prepared(path).map_err(map_session_error)?;
        if is_empty {
            write_json(
                &mut writer,
                &SessionEntry::Meta {
                    schema_version: SESSION_SCHEMA_VERSION,
                },
            )?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(StoreState {
                writer: Some(writer),
                thread_id: resumed.thread_id,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Io("poisoned thread store lock".into()))
    }
}

#[async_trait::async_trait]
impl ThreadStore for JsonlThreadStore {
    async fn create(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let StoreState {
            writer,
            thread_id: bound,
        } = &mut *state;
        let Some(writer) = writer.as_mut() else {
            return Err(StoreError::Closed);
        };
        match *bound {
            Some(held) if held != *thread_id => Err(StoreError::ThreadMismatch {
                holds: held,
                given: *thread_id,
            }),
            Some(_) => Ok(()),
            None => {
                write_json(
                    writer,
                    &ThreadLine::ThreadMeta {
                        thread_id: *thread_id,
                        runtime_version: THREAD_RUNTIME_VERSION,
                    },
                )?;
                *bound = Some(*thread_id);
                Ok(())
            }
        }
    }

    async fn append(&self, event: &ThreadEvent) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let StoreState {
            writer,
            thread_id: bound,
        } = &mut *state;
        let Some(writer) = writer.as_mut() else {
            return Err(StoreError::Closed);
        };
        if let Some(held) = *bound
            && held != event.thread_id
        {
            return Err(StoreError::ThreadMismatch {
                holds: held,
                given: event.thread_id,
            });
        }
        write_json(writer, &ThreadLine::ThreadEvent(event.clone()))
    }

    async fn flush(&self) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let Some(writer) = state.writer.as_mut() else {
            return Err(StoreError::Closed);
        };
        writer.sync_all().map_err(map_session_error)
    }

    async fn read(&self) -> Result<ThreadSnapshot, StoreError> {
        // Reads from the path, not from the writer: a closed store must still
        // be readable, and a fork copies bytes, never in-memory state.
        let resumed = resume_file(&self.path).map_err(map_session_error)?;
        Ok(ThreadSnapshot {
            thread_id: resumed.thread_id,
            messages: resumed.messages,
            events: resumed.thread_events,
            schema_version: resumed.schema_version,
            skipped_partial: resumed.skipped_partial,
        })
    }

    async fn close(&self) -> Result<(), StoreError> {
        // Dropping the writer releases the file lock. Idempotent.
        self.lock()?.writer = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agent_core::message::Message;
    use agent_runtime::event::ThreadEventPayload;
    use agent_runtime::id::{EventId, SequentialIds, TurnId};
    use agent_runtime::lifecycle::TurnState;
    use agent_runtime::store::contract::assert_thread_store_contract;

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis_thread_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("session.jsonl")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn event(ids: &SequentialIds, thread_id: ThreadId, seq: u64) -> ThreadEvent {
        ThreadEvent {
            event_id: EventId::generate(ids),
            thread_id,
            seq,
            at_ms: seq,
            payload: ThreadEventPayload::TurnStateChanged {
                turn_id: TurnId::generate(ids),
                from: None,
                to: TurnState::Queued,
                cause: None,
            },
        }
    }

    #[tokio::test]
    async fn jsonl_adapter_satisfies_the_store_contract() {
        let path = tmp("contract");
        let store = JsonlThreadStore::open(&path).unwrap();
        assert_thread_store_contract(&store).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn reads_a_v1_session_that_only_holds_meta_messages_and_compactions() {
        let path = tmp("v1-read");
        let lines = [
            r#"{"entry":"meta","schema_version":1}"#.to_string(),
            serde_json::to_string(&SessionEntry::Message(Message::user("un"))).unwrap(),
            serde_json::to_string(&SessionEntry::Message(Message::assistant_text("deux"))).unwrap(),
            serde_json::to_string(&SessionEntry::Message(Message::user("trois"))).unwrap(),
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let store = JsonlThreadStore::open(&path).unwrap();
        let snapshot = store.read().await.unwrap();
        assert_eq!(snapshot.thread_id, None, "a v1 session is not bound yet");
        assert_eq!(snapshot.schema_version, Some(1));
        let texts: Vec<String> = snapshot.messages.iter().map(Message::text).collect();
        assert_eq!(texts, vec!["un", "deux", "trois"]);
        assert!(snapshot.events.is_empty());
        cleanup(&path);
    }

    #[tokio::test]
    async fn continuing_a_v1_session_keeps_its_prefix_byte_identical() {
        let path = tmp("v1-continue");
        let lines = [
            r#"{"entry":"meta","schema_version":1}"#.to_string(),
            serde_json::to_string(&SessionEntry::Message(Message::user("historique"))).unwrap(),
        ];
        let prefix = format!("{}\n", lines.join("\n"));
        std::fs::write(&path, &prefix).unwrap();
        let prefix_bytes = std::fs::read(&path).unwrap();

        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let store = JsonlThreadStore::open(&path).unwrap();
        store.create(&thread_id).await.unwrap();
        store.append(&event(&ids, thread_id, 1)).await.unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            &after[..prefix_bytes.len()],
            &prefix_bytes[..],
            "the v1 prefix must not be rewritten"
        );
        assert!(after.len() > prefix_bytes.len());

        let snapshot = store.read().await.unwrap();
        assert_eq!(snapshot.thread_id, Some(thread_id));
        assert_eq!(snapshot.messages.len(), 1, "v1 messages stay visible");
        assert_eq!(snapshot.events.len(), 1, "v2 events replay too");
        assert_eq!(snapshot.next_seq(), 2);
        cleanup(&path);
    }

    #[tokio::test]
    async fn a_partial_last_line_is_truncated_at_the_last_valid_offset() {
        let path = tmp("partial");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        {
            let store = JsonlThreadStore::open(&path).unwrap();
            store.create(&thread_id).await.unwrap();
            store.append(&event(&ids, thread_id, 1)).await.unwrap();
            store.close().await.unwrap();
        }
        let valid = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{valid}{{\"entry\":\"thread_ev")).unwrap();

        let store = JsonlThreadStore::open(&path).unwrap();
        let snapshot = store.read().await.unwrap();
        assert_eq!(snapshot.events.len(), 1, "the partial line is dropped");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            valid,
            "the file is truncated back to the last valid offset"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn a_corruption_in_the_middle_names_its_offset_and_acknowledges_nothing() {
        let path = tmp("corrupt-middle");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        {
            let store = JsonlThreadStore::open(&path).unwrap();
            store.create(&thread_id).await.unwrap();
            store.append(&event(&ids, thread_id, 1)).await.unwrap();
            store.close().await.unwrap();
        }
        let valid = std::fs::read_to_string(&path).unwrap();
        let offset = valid.len() as u64;
        std::fs::write(&path, format!("{valid}GARBAGE\n{valid}")).unwrap();

        let err = JsonlThreadStore::open(&path)
            .err()
            .expect("a corrupt middle line must refuse to open");
        assert!(
            matches!(&err, StoreError::Corrupt { offset: at, .. } if *at == offset),
            "expected Corrupt at offset {offset}, got {err:?}"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn a_closed_store_refuses_appends_but_stays_readable() {
        let path = tmp("closed");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let store = JsonlThreadStore::open(&path).unwrap();
        store.create(&thread_id).await.unwrap();
        let appended = event(&ids, thread_id, 1);
        store.append(&appended).await.unwrap();
        store.close().await.unwrap();

        assert!(matches!(
            store.append(&appended).await,
            Err(StoreError::Closed)
        ));
        let snapshot = store.read().await.unwrap();
        assert_eq!(snapshot.events, vec![appended]);
        cleanup(&path);
    }

    #[tokio::test]
    async fn an_event_from_another_thread_is_refused_and_writes_nothing() {
        let path = tmp("mismatch");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let other = ThreadId::generate(&ids);
        let store = JsonlThreadStore::open(&path).unwrap();
        store.create(&thread_id).await.unwrap();
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            store.append(&event(&ids, other, 1)).await,
            Err(StoreError::ThreadMismatch { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        cleanup(&path);
    }
}
