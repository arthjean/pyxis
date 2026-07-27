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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::session::{SessionEntry, SessionError};
use agent_runtime::event::{THREAD_RUNTIME_VERSION, ThreadEvent};
use agent_runtime::id::{EventId, ID_BYTES, ThreadId};
use agent_runtime::store::{ForkPoint, StoreError, ThreadSnapshot, ThreadStore};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    LogLine, SESSION_SCHEMA_VERSION, ThreadLine, WriterState, open_prepared, parse_log_line,
    resume_file, write_buf_locked,
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

    /// Thread this log belongs to: the one it is already bound to, or the one
    /// derived for a session that predates the runtime (US-009 AC4).
    ///
    /// Deriving does NOT write. `ThreadHandle::start` materializes the binding
    /// line on its first `create`, and from then on the identifier comes from
    /// the file itself, so moving or renaming the session no longer changes it.
    pub fn thread_id(&self) -> Result<ThreadId, StoreError> {
        if let Some(bound) = self.lock()?.thread_id {
            return Ok(bound);
        }
        derive_thread_id(&self.path)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Io("poisoned thread store lock".into()))
    }
}

/// Stable `ThreadId` of a session file that carries no binding line yet.
///
/// Hashes the canonical path AND the first durable line: two copies of the same
/// transcript in two directories are two threads, and an empty file in a given
/// place always derives the same identifier. The path dependency only lasts
/// until the binding line is written, which is the whole point of materializing
/// it once (PRD open question, US-009).
fn derive_thread_id(path: &Path) -> Result<ThreadId, StoreError> {
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let first_line = match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(StoreError::Io(e.to_string())),
    };

    let mut hasher = Sha256::new();
    hasher.update(b"pyxis-thread-v1\0");
    hasher.update(canonical.as_bytes());
    hasher.update(b"\0");
    hasher.update(first_line.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; ID_BYTES];
    bytes.copy_from_slice(&digest[..ID_BYTES]);
    Ok(ThreadId::from_bytes(bytes))
}

/// Byte length of the prefix ending with the line that carries `event_id`,
/// newline included. `None` when the log does not carry that event.
fn prefix_through_event(content: &str, event_id: EventId) -> Option<usize> {
    let mut start = 0usize;
    while start < content.len() {
        let (line, end) = match content[start..].find('\n') {
            Some(rel) => (&content[start..start + rel], start + rel + 1),
            None => (&content[start..], content.len()),
        };
        if let Ok(LogLine::Thread(ThreadLine::ThreadEvent(event))) =
            parse_log_line(line.strip_suffix('\r').unwrap_or(line))
            && event.event_id == event_id
        {
            return Some(end);
        }
        start = end;
    }
    None
}

/// Writes the branch file aside and moves it into place (US-010 AC5).
///
/// Staged then renamed: a failure mid-copy leaves no readable child, and the
/// parent is never opened for writing at all. The child's own binding line goes
/// LAST, after the inherited prefix, so it is the binding the reader keeps.
fn materialize(parent: &Path, at: &ForkPoint) -> Result<PathBuf, StoreError> {
    let io = |e: std::io::Error| StoreError::Io(e.to_string());
    let content = std::fs::read_to_string(parent).map_err(io)?;
    let cut = prefix_through_event(&content, at.fork_event_id).ok_or(StoreError::NoSuchEvent {
        event_id: at.fork_event_id,
    })?;
    let binding = serde_json::to_string(&ThreadLine::ThreadMeta {
        thread_id: at.child_thread_id,
        runtime_version: THREAD_RUNTIME_VERSION,
        origin: Some(at.origin()),
    })
    .map_err(|e| StoreError::Serde(e.to_string()))?;

    let dir = parent.parent().unwrap_or_else(|| Path::new("."));
    let child = dir.join(format!("{}.jsonl", at.child_thread_id));
    if child.exists() {
        return Err(StoreError::Io(format!(
            "branch file {} already exists",
            child.display()
        )));
    }
    let staging = dir.join(format!(".{}.partial", at.child_thread_id));

    let staged = (|| -> Result<(), StoreError> {
        let mut file = std::fs::File::create(&staging).map_err(io)?;
        let prefix = &content[..cut];
        file.write_all(prefix.as_bytes()).map_err(io)?;
        if !prefix.ends_with('\n') {
            file.write_all(b"\n").map_err(io)?;
        }
        file.write_all(binding.as_bytes()).map_err(io)?;
        file.write_all(b"\n").map_err(io)?;
        file.flush().map_err(io)?;
        file.sync_all().map_err(io)
    })();
    if let Err(err) = staged {
        let _ = std::fs::remove_file(&staging);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&staging, &child) {
        let _ = std::fs::remove_file(&staging);
        return Err(io(err));
    }
    Ok(child)
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
                        // A binding written here is the birth of a root thread.
                        // A branch gets its provenance from `materialize`, which
                        // writes the binding line itself.
                        origin: None,
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
            origin: resumed.origin,
        })
    }

    async fn fork(&self, at: &ForkPoint) -> Result<Arc<dyn ThreadStore>, StoreError> {
        let child_path = {
            let mut state = self.lock()?;
            let StoreState {
                writer,
                thread_id: bound,
            } = &mut *state;
            let Some(writer) = writer.as_mut() else {
                return Err(StoreError::Closed);
            };
            if let Some(held) = *bound
                && held != at.parent_thread_id
            {
                return Err(StoreError::ThreadMismatch {
                    holds: held,
                    given: at.parent_thread_id,
                });
            }
            // AC1: data AND metadata out before the copy reads the file back.
            // Every line was already `sync_data`'d; this is what makes the size
            // the copy sees the size the parent acknowledged.
            writer.sync_all().map_err(map_session_error)?;
            materialize(&self.path, at)?
        };
        // Opened AFTER the lock is released: the child is a different file, and
        // holding both locks at once would order two independent threads.
        JsonlThreadStore::open(&child_path).map(|store| Arc::new(store) as Arc<dyn ThreadStore>)
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
    use agent_runtime::store::contract::{
        assert_thread_fork_contract, assert_thread_store_contract,
    };

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
                context: None,
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
    async fn jsonl_adapter_satisfies_the_fork_contract() {
        let path = tmp("fork-contract");
        let store = JsonlThreadStore::open(&path).unwrap();
        assert_thread_fork_contract(&store).await;
        cleanup(&path);
    }

    /// US-009 AC4: a session written before the runtime existed gets a stable
    /// identifier, materialized exactly once, without losing a message.
    #[tokio::test]
    async fn a_v1_session_derives_a_stable_thread_id_materialized_once() {
        let path = tmp("v1-derive");
        let lines = [
            r#"{"entry":"meta","schema_version":1}"#.to_string(),
            serde_json::to_string(&SessionEntry::Message(Message::user("historique"))).unwrap(),
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let derived = {
            let store = JsonlThreadStore::open(&path).unwrap();
            let derived = store.thread_id().unwrap();
            assert_eq!(
                store.thread_id().unwrap(),
                derived,
                "deriving twice yields the same identifier"
            );
            // Not written yet: inspecting a session must not modify it.
            assert!(resume_file(&path).unwrap().thread_id.is_none());
            store.create(&derived).await.unwrap();
            store.close().await.unwrap();
            derived
        };

        let bindings = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| l.contains("\"entry\":\"thread_meta\""))
            .count();
        assert_eq!(bindings, 1, "the binding line is written once");

        let reopened = JsonlThreadStore::open(&path).unwrap();
        assert_eq!(reopened.thread_id().unwrap(), derived);
        let snapshot = reopened.read().await.unwrap();
        assert_eq!(snapshot.thread_id, Some(derived));
        assert_eq!(snapshot.messages.len(), 1, "the v1 message stays visible");
        assert_eq!(snapshot.messages[0].text(), "historique");
        cleanup(&path);
    }

    /// The derivation depends on the CONTENT, not only on the location: two
    /// different transcripts in the same place are two threads.
    #[test]
    fn two_v1_sessions_with_different_content_derive_different_ids() {
        let path = tmp("v1-distinct");
        std::fs::write(&path, "{\"entry\":\"meta\",\"schema_version\":1}\n").unwrap();
        let first = JsonlThreadStore::open(&path).unwrap().thread_id().unwrap();

        let other = path.with_file_name("autre.jsonl");
        std::fs::write(&other, "{\"entry\":\"meta\",\"schema_version\":1}\n").unwrap();
        let second = JsonlThreadStore::open(&other).unwrap().thread_id().unwrap();
        assert_ne!(first, second, "same content, another path: another thread");

        std::fs::write(
            &other,
            format!(
                "{}\n",
                serde_json::to_string(&SessionEntry::Message(Message::user("autre"))).unwrap()
            ),
        )
        .unwrap();
        let third = JsonlThreadStore::open(&other).unwrap().thread_id().unwrap();
        assert_ne!(
            second, third,
            "same path, another first line: another thread"
        );
        cleanup(&path);
    }

    /// US-010 AC4: the branch is materialized, so it survives its source. And
    /// AC5: the source keeps its bytes when the fork is refused.
    #[tokio::test]
    async fn a_branch_outlives_the_parent_it_was_cut_from() {
        let path = tmp("fork-orphan");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let child_thread_id = ThreadId::generate(&ids);
        let fork_turn_id = TurnId::generate(&ids);

        let store = JsonlThreadStore::open(&path).unwrap();
        store.create(&thread_id).await.unwrap();
        let cut = ThreadEvent {
            event_id: EventId::generate(&ids),
            thread_id,
            seq: 1,
            at_ms: 1,
            payload: ThreadEventPayload::TurnStateChanged {
                turn_id: fork_turn_id,
                from: Some(TurnState::Running),
                to: TurnState::Completed,
                cause: None,
                context: None,
            },
        };
        store.append(&cut).await.unwrap();
        let parent_bytes = std::fs::read(&path).unwrap();

        // An unknown cut publishes nothing and leaves the parent untouched.
        let unknown = ForkPoint {
            parent_thread_id: thread_id,
            child_thread_id,
            fork_turn_id,
            fork_event_id: EventId::generate(&ids),
        };
        assert!(matches!(
            store.fork(&unknown).await,
            Err(StoreError::NoSuchEvent { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), parent_bytes);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no partial branch was left behind"
        );

        let point = ForkPoint {
            fork_event_id: cut.event_id,
            ..unknown
        };
        let child = store.fork(&point).await.unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            parent_bytes,
            "forking writes nothing into the source"
        );

        // The parent disappears; the branch stays readable on its own.
        store.close().await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let snapshot = child.read().await.unwrap();
        assert_eq!(snapshot.thread_id, Some(child_thread_id));
        assert_eq!(snapshot.origin, Some(point.origin()));
        assert_eq!(snapshot.events, vec![cut]);
        cleanup(&path);
    }

    /// US-009 AC6 / edge case #10: a corruption in the middle refuses the resume
    /// and hands back the offset, so no thread starts on a partial state. The
    /// `open` path is covered above; this one proves the READ path agrees.
    #[test]
    fn a_corrupt_log_names_its_offset_before_any_thread_opens_it() {
        let path = tmp("corrupt-resume");
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        let offset = valid.len() as u64 + 1;
        std::fs::write(&path, format!("{valid}\nGARBAGE\n{valid}\n")).unwrap();

        let err = resume_file(&path).unwrap_err();
        assert!(
            matches!(&err, SessionError::Corrupt { offset: at, .. } if *at == offset),
            "expected a corruption at {offset}, got {err:?}"
        );
        assert!(JsonlThreadStore::open(&path).is_err());
        cleanup(&path);
    }

    /// US-009 AC5 and US-010 AC6, on the reference machine, in release:
    /// `cargo test --release -p agent-session -- --ignored --nocapture`.
    ///
    /// Ignored by default: a debug build measures the interpreter, not the
    /// store, and the targets of the PRD are release targets.
    #[tokio::test]
    #[ignore = "benchmark: run in release"]
    async fn ten_thousand_events_replay_and_fork_inside_the_budget() {
        use std::time::Instant;

        let path = tmp("bench-10k");
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let store = JsonlThreadStore::open(&path).unwrap();
        store.create(&thread_id).await.unwrap();
        let mut cut = None;
        for seq in 1..=10_000u64 {
            let e = event(&ids, thread_id, seq);
            store.append(&e).await.unwrap();
            if seq == 10_000 {
                cut = Some(e);
            }
        }

        let mut samples: Vec<u128> = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let snapshot = store.read().await.unwrap();
            samples.push(started.elapsed().as_micros());
            assert_eq!(snapshot.events.len(), 10_000);
        }
        samples.sort_unstable();
        let p95 = samples[(samples.len() as f64 * 0.95).ceil() as usize - 1];
        println!("resume 10k events: p95 = {:.1} ms", p95 as f64 / 1000.0);
        assert!(p95 < 500_000, "p95 above the 500 ms budget: {p95} µs");

        let cut = cut.unwrap();
        let bytes = std::fs::metadata(&path).unwrap().len();
        let started = Instant::now();
        let child = store
            .fork(&ForkPoint {
                parent_thread_id: thread_id,
                child_thread_id: ThreadId::generate(&ids),
                fork_turn_id: cut.turn_id().unwrap(),
                fork_event_id: cut.event_id,
            })
            .await
            .unwrap();
        let forked_in = started.elapsed();
        println!(
            "fork of a 10k-event session: {:.1} ms, {bytes} bytes copied",
            forked_in.as_secs_f64() * 1000.0
        );
        assert_eq!(child.read().await.unwrap().events.len(), 10_000);
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
