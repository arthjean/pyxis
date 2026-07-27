//! `ThreadStore`: durable log of a thread (US-003).
//!
//! Two real adapters justify the trait: the JSONL adapter of `agent-session`
//! (production, same file as the v1 transcript) and `MemoryThreadStore` (tests
//! and `--ephemeral`). `contract::assert_thread_store_contract` is the single
//! test both must satisfy, so "durable before acknowledgement" is verified once
//! and not re-described per adapter.
//!
//! The store is DUMB on purpose: it never mints an identifier and never decides
//! a transition. The runtime owns the `IdGenerator` and the lifecycle; the store
//! only guarantees that what it accepted survived.

use std::sync::Arc;

use agent_core::message::Message;

use crate::event::{ForkOrigin, ThreadEvent};
use crate::id::{EventId, ThreadId, TurnId};

/// Named failures of a store. Every variant is a refusal to acknowledge: a
/// caller that gets one must NOT treat the operation as accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("thread store io: {0}")]
    Io(String),
    #[error("thread store serde: {0}")]
    Serde(String),
    /// A line in the middle of the log is unreadable. Resume must refuse
    /// rather than start on a partial state (edge case #10).
    #[error("corrupt thread log at offset {offset}: {detail}")]
    Corrupt { offset: u64, detail: String },
    /// A previous append failed after touching the file: the writer can no
    /// longer promise ordering, so it refuses everything until reopened.
    #[error("thread store poisoned by a failed append; reopen the thread")]
    Poisoned,
    #[error("thread store is closed")]
    Closed,
    /// The store is bound to a thread and was handed another one. Prevents two
    /// threads from sharing a durable log.
    #[error("thread store holds {holds}, got {given}")]
    ThreadMismatch { holds: ThreadId, given: ThreadId },
    /// A fork named a cut this log does not carry. Refusing is what keeps
    /// `fork` from materializing an arbitrary prefix (US-010 AC5).
    #[error("no event {event_id} in this thread log")]
    NoSuchEvent { event_id: EventId },
}

/// Where a branch is cut and what it must remember about its source (US-010).
///
/// The runtime resolves it against the DURABLE log before calling the store:
/// `fork_event_id` is always the terminal transition of `fork_turn_id`, so the
/// materialized prefix ends on a complete turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkPoint {
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub fork_turn_id: TurnId,
    pub fork_event_id: EventId,
}

impl ForkPoint {
    /// Provenance the child records about this cut.
    pub fn origin(&self) -> ForkOrigin {
        ForkOrigin {
            parent_thread_id: self.parent_thread_id,
            fork_turn_id: self.fork_turn_id,
            fork_event_id: self.fork_event_id,
        }
    }
}

/// Durable state rebuilt from a thread log. Carries BOTH formats: the v1
/// messages and the v2 orchestration events (US-003 AC3/AC4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadSnapshot {
    /// `None` for a v1 session that has never been bound to a thread.
    pub thread_id: Option<ThreadId>,
    pub messages: Vec<Message>,
    pub events: Vec<ThreadEvent>,
    pub schema_version: Option<u32>,
    /// True when a partial trailing line (crash mid-write) was truncated.
    pub skipped_partial: bool,
    /// Branch this log was cut from, when it is one (US-010 AC2).
    pub origin: Option<ForkOrigin>,
}

impl ThreadSnapshot {
    /// Sequence number the next event must carry.
    pub fn next_seq(&self) -> u64 {
        self.events.last().map_or(1, |e| e.seq.saturating_add(1))
    }
}

/// Durable log of ONE thread.
///
/// `create`, `append` and `flush` fail once the store is closed. `read` stays
/// valid after `close`: reading durable state is not an admission, and the DoD
/// of EP-001 requires rebuilding a thread from a closed store.
#[async_trait::async_trait]
pub trait ThreadStore: Send + Sync {
    /// Binds the log to `thread_id`. Idempotent for the same identifier,
    /// `ThreadMismatch` for another one.
    async fn create(&self, thread_id: &ThreadId) -> Result<(), StoreError>;

    /// Appends an event. Returns only once the event is durable.
    async fn append(&self, event: &ThreadEvent) -> Result<(), StoreError>;

    /// Forces every pending byte (data AND metadata) out. Used before copying a
    /// prefix at fork time (US-010).
    async fn flush(&self) -> Result<(), StoreError>;

    /// Rebuilds the durable state.
    async fn read(&self) -> Result<ThreadSnapshot, StoreError>;

    /// Materializes the durable prefix up to `at.fork_event_id` INCLUDED as an
    /// independent log bound to `at.child_thread_id`, and returns it open.
    ///
    /// Flushes first, so the copy sees every byte the parent acknowledged. All
    /// or nothing: a failure publishes no child and leaves the source
    /// byte-identical, which is what makes a failed `/fork` a no-op rather than
    /// a half-branch (US-010 AC5). Materialized and not referenced on purpose:
    /// a child survives the deletion of its parent (AC4).
    async fn fork(&self, at: &ForkPoint) -> Result<Arc<dyn ThreadStore>, StoreError>;

    /// Releases the log. Idempotent.
    async fn close(&self) -> Result<(), StoreError>;
}

/// In-memory adapter: tests and `--ephemeral` (US-018 AC4, no file at all).
#[derive(Debug, Default)]
pub struct MemoryThreadStore {
    state: std::sync::Mutex<MemoryState>,
}

#[derive(Debug, Default)]
struct MemoryState {
    thread_id: Option<ThreadId>,
    events: Vec<ThreadEvent>,
    origin: Option<ForkOrigin>,
    closed: bool,
}

impl MemoryThreadStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Branch of another memory store: bound, pre-filled with the copied prefix
    /// and carrying its provenance.
    fn branched(thread_id: ThreadId, events: Vec<ThreadEvent>, origin: ForkOrigin) -> Self {
        Self {
            state: std::sync::Mutex::new(MemoryState {
                thread_id: Some(thread_id),
                events,
                origin: Some(origin),
                closed: false,
            }),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Io("poisoned memory store lock".into()))
    }
}

#[async_trait::async_trait]
impl ThreadStore for MemoryThreadStore {
    async fn create(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(StoreError::Closed);
        }
        match state.thread_id {
            Some(held) if held != *thread_id => Err(StoreError::ThreadMismatch {
                holds: held,
                given: *thread_id,
            }),
            Some(_) => Ok(()),
            None => {
                state.thread_id = Some(*thread_id);
                Ok(())
            }
        }
    }

    async fn append(&self, event: &ThreadEvent) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(StoreError::Closed);
        }
        if let Some(held) = state.thread_id
            && held != event.thread_id
        {
            return Err(StoreError::ThreadMismatch {
                holds: held,
                given: event.thread_id,
            });
        }
        state.events.push(event.clone());
        Ok(())
    }

    async fn flush(&self) -> Result<(), StoreError> {
        let state = self.lock()?;
        if state.closed {
            return Err(StoreError::Closed);
        }
        Ok(())
    }

    async fn read(&self) -> Result<ThreadSnapshot, StoreError> {
        let state = self.lock()?;
        Ok(ThreadSnapshot {
            thread_id: state.thread_id,
            messages: Vec::new(),
            events: state.events.clone(),
            schema_version: None,
            skipped_partial: false,
            origin: state.origin,
        })
    }

    async fn fork(&self, at: &ForkPoint) -> Result<Arc<dyn ThreadStore>, StoreError> {
        let state = self.lock()?;
        if state.closed {
            return Err(StoreError::Closed);
        }
        if let Some(held) = state.thread_id
            && held != at.parent_thread_id
        {
            return Err(StoreError::ThreadMismatch {
                holds: held,
                given: at.parent_thread_id,
            });
        }
        let cut = state
            .events
            .iter()
            .position(|e| e.event_id == at.fork_event_id)
            .ok_or(StoreError::NoSuchEvent {
                event_id: at.fork_event_id,
            })?;
        Ok(Arc::new(MemoryThreadStore::branched(
            at.child_thread_id,
            state.events[..=cut].to_vec(),
            at.origin(),
        )))
    }

    async fn close(&self) -> Result<(), StoreError> {
        self.lock()?.closed = true;
        Ok(())
    }
}

pub mod contract {
    //! Shared conformance test for every `ThreadStore` adapter (US-003 AC1).
    //!
    //! Public on purpose: `agent-session` runs the exact same assertions on its
    //! JSONL adapter as `agent-runtime` runs on the memory adapter.
    //!
    //! Test harness compiled into the library (a `#[cfg(test)]` module would not
    //! be visible from `agent-session`), so it asserts and unwraps like a test.
    #![allow(clippy::expect_used)]

    use super::{ForkPoint, StoreError, ThreadStore};
    use crate::event::{ThreadEvent, ThreadEventPayload};
    use crate::id::{EventId, IdGenerator, SequentialIds, ThreadId, TurnId};
    use crate::lifecycle::TurnState;

    fn event(
        ids: &dyn IdGenerator,
        thread_id: ThreadId,
        seq: u64,
        payload: ThreadEventPayload,
    ) -> ThreadEvent {
        ThreadEvent {
            event_id: EventId::generate(ids),
            thread_id,
            seq,
            at_ms: 1_000 + seq,
            payload,
        }
    }

    /// Runs the full contract against `store`, which must be freshly opened and
    /// empty. Panics with the failing assertion, like any test helper.
    pub async fn assert_thread_store_contract(store: &dyn ThreadStore) {
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let other_thread = ThreadId::generate(&ids);
        let turn_id = TurnId::generate(&ids);

        // 1. An empty store reads as an empty, unbound snapshot.
        let snapshot = store.read().await.expect("read an empty store");
        assert_eq!(snapshot.thread_id, None, "a fresh store is unbound");
        assert!(snapshot.events.is_empty(), "a fresh store has no event");
        assert_eq!(snapshot.next_seq(), 1);

        // 2. `create` binds the log and is idempotent for the same thread.
        store.create(&thread_id).await.expect("create");
        store
            .create(&thread_id)
            .await
            .expect("create is idempotent");
        let snapshot = store.read().await.expect("read after create");
        assert_eq!(snapshot.thread_id, Some(thread_id));

        // 3. A bound store refuses another thread.
        let rebind = store.create(&other_thread).await;
        assert!(
            matches!(
                &rebind,
                Err(StoreError::ThreadMismatch { holds, given })
                    if *holds == thread_id && *given == other_thread
            ),
            "expected ThreadMismatch on rebinding, got {rebind:?}"
        );

        // 4. Appended events come back in order, byte-for-byte.
        let appended = vec![
            event(&ids, thread_id, 1, ThreadEventPayload::ThreadCreated),
            event(
                &ids,
                thread_id,
                2,
                ThreadEventPayload::InputSubmitted {
                    turn_id,
                    client_message_id: Some("cli-1".into()),
                    text: "premier tour".into(),
                },
            ),
            event(
                &ids,
                thread_id,
                3,
                ThreadEventPayload::TurnStateChanged {
                    turn_id,
                    from: Some(TurnState::Queued),
                    to: TurnState::Running,
                    cause: None,
                    context: None,
                },
            ),
        ];
        for e in &appended {
            store.append(e).await.expect("append");
        }
        let snapshot = store.read().await.expect("read after append");
        assert_eq!(snapshot.events, appended, "events must replay in order");
        assert_eq!(snapshot.next_seq(), 4);

        // 5. An event belonging to another thread is refused, and refusing it
        //    persists nothing.
        let foreign = event(&ids, other_thread, 4, ThreadEventPayload::ThreadCreated);
        assert!(matches!(
            store.append(&foreign).await,
            Err(StoreError::ThreadMismatch { .. })
        ));
        let snapshot = store.read().await.expect("read after a refused append");
        assert_eq!(snapshot.events.len(), 3, "a refused append writes nothing");

        // 6. Flush then close. After close, admission is refused but the durable
        //    state stays readable.
        store.flush().await.expect("flush");
        store.close().await.expect("close");
        store.close().await.expect("close is idempotent");
        assert!(matches!(
            store.append(&appended[0]).await,
            Err(StoreError::Closed)
        ));
        assert!(matches!(
            store.create(&thread_id).await,
            Err(StoreError::Closed)
        ));
        assert!(matches!(store.flush().await, Err(StoreError::Closed)));

        let snapshot = store.read().await.expect("read a closed store");
        assert_eq!(snapshot.thread_id, Some(thread_id));
        assert_eq!(snapshot.events, appended);
    }

    /// Fork contract (US-010), run on a freshly opened empty store. Kept apart
    /// from [`assert_thread_store_contract`] because that one ends on a closed
    /// store, and a fork is an operation of a LIVE one.
    ///
    /// The caller owns the cleanup of the returned child.
    pub async fn assert_thread_fork_contract(store: &dyn ThreadStore) {
        let ids = SequentialIds::starting_at(1_000);
        let thread_id = ThreadId::generate(&ids);
        let first_turn = TurnId::generate(&ids);
        let second_turn = TurnId::generate(&ids);
        store.create(&thread_id).await.expect("create");

        let mut log = Vec::new();
        for (seq, payload) in [
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id: first_turn,
                client_message_id: None,
                text: "premier".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: first_turn,
                from: Some(TurnState::Queued),
                to: TurnState::Completed,
                cause: None,
                context: None,
            },
            ThreadEventPayload::InputSubmitted {
                turn_id: second_turn,
                client_message_id: None,
                text: "second".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let e = event(&ids, thread_id, seq as u64 + 1, payload);
            store.append(&e).await.expect("append");
            log.push(e);
        }
        // Cut on the terminal of the first turn: everything after it belongs to
        // the parent alone.
        let cut = log[2].event_id;
        let point = ForkPoint {
            parent_thread_id: thread_id,
            child_thread_id: ThreadId::generate(&ids),
            fork_turn_id: first_turn,
            fork_event_id: cut,
        };

        // 1. An unknown cut is refused and materializes nothing.
        let unknown = ForkPoint {
            fork_event_id: EventId::generate(&ids),
            ..point
        };
        assert!(
            matches!(
                store.fork(&unknown).await,
                Err(StoreError::NoSuchEvent { .. })
            ),
            "an unknown fork point must be refused"
        );

        // 2. The child carries the prefix THROUGH the cut, its own binding and
        //    its provenance.
        let child = store.fork(&point).await.expect("fork");
        let snapshot = child.read().await.expect("read the child");
        assert_eq!(snapshot.thread_id, Some(point.child_thread_id));
        assert_eq!(snapshot.origin, Some(point.origin()));
        assert_eq!(
            snapshot.events,
            log[..3],
            "the child holds the prefix up to the cut, and nothing after it"
        );

        // 3. Parent and child diverge without cross-writes.
        let child_event = event(
            &ids,
            point.child_thread_id,
            snapshot.next_seq(),
            ThreadEventPayload::TurnStateChanged {
                turn_id: second_turn,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
        );
        child
            .append(&child_event)
            .await
            .expect("append to the child");
        let parent_after = store.read().await.expect("read the parent");
        assert_eq!(
            parent_after.events, log,
            "writing to the child adds nothing to the parent"
        );
        assert_eq!(
            child.read().await.expect("re-read the child").events.len(),
            4,
            "the child keeps its own event"
        );
        // 4. The child refuses an event addressed to its parent.
        assert!(matches!(
            child.append(&log[3]).await,
            Err(StoreError::ThreadMismatch { .. })
        ));
        child.close().await.expect("close the child");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ThreadEventPayload;
    use crate::id::{EventId, SequentialIds};

    #[tokio::test]
    async fn memory_adapter_satisfies_the_store_contract() {
        contract::assert_thread_store_contract(&MemoryThreadStore::new()).await;
    }

    #[tokio::test]
    async fn memory_adapter_satisfies_the_fork_contract() {
        contract::assert_thread_fork_contract(&MemoryThreadStore::new()).await;
    }

    #[tokio::test]
    async fn an_unbound_memory_store_accepts_the_first_thread_it_sees() {
        let ids = SequentialIds::new();
        let store = MemoryThreadStore::new();
        let thread_id = ThreadId::generate(&ids);
        store
            .append(&ThreadEvent {
                event_id: EventId::generate(&ids),
                thread_id,
                seq: 1,
                at_ms: 0,
                payload: ThreadEventPayload::ThreadCreated,
            })
            .await
            .expect("append without create");
        assert_eq!(store.read().await.unwrap().events.len(), 1);
    }
}
