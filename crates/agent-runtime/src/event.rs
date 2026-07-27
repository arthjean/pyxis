//! Durable orchestration events (US-003).
//!
//! Exactly the four operations FR-05 requires to be durable before an
//! acknowledgement: an input, a state change, a fork and an agent relation.
//! Plus the thread creation itself, which is what binds a log file to a
//! `ThreadId`.
//!
//! These events are ADDITIVE to the v1 session format: they are new discriminated
//! entries in the same JSONL file. A v1 reader maps them to
//! `SessionEntry::Unknown` and skips them, so the schema version is deliberately
//! NOT bumped (bumping it would make new files unreadable by older binaries,
//! which reject `schema_version > 1`).

use serde::{Deserialize, Serialize};

use crate::context::TurnContext;
use crate::id::{AgentId, EventId, ThreadId, TurnId};
use crate::lifecycle::TurnState;

/// `entry` tag of a thread binding line in the JSONL log.
pub const THREAD_META_ENTRY: &str = "thread_meta";
/// `entry` tag of an orchestration event line in the JSONL log.
pub const THREAD_EVENT_ENTRY: &str = "thread_event";
/// Version of the orchestration layer written in the binding line.
pub const THREAD_RUNTIME_VERSION: u32 = 1;

/// Provenance of a materialized branch (US-010 AC2).
///
/// Written in the binding line of the CHILD, never in the parent: that is what
/// keeps a branch readable after its source is deleted or archived. A fork of a
/// fork overwrites nothing: each log carries the origin of its own cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOrigin {
    pub parent_thread_id: ThreadId,
    pub fork_turn_id: TurnId,
    pub fork_event_id: EventId,
}

/// One durable orchestration event. `seq` is monotonic per thread and starts at
/// 1: it orders events inside a file without depending on the clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub event_id: EventId,
    pub thread_id: ThreadId,
    pub seq: u64,
    /// Epoch ms from the injected clock. Ordering never relies on it.
    pub at_ms: u64,
    pub payload: ThreadEventPayload,
}

impl ThreadEvent {
    /// Owning turn, when the event belongs to one.
    pub fn turn_id(&self) -> Option<TurnId> {
        match &self.payload {
            ThreadEventPayload::InputSubmitted { turn_id, .. }
            | ThreadEventPayload::TurnStateChanged { turn_id, .. }
            | ThreadEventPayload::Forked {
                fork_turn_id: turn_id,
                ..
            } => Some(*turn_id),
            ThreadEventPayload::ThreadCreated | ThreadEventPayload::AgentLinked { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadEventPayload {
    /// First event of a thread. Never emitted twice for the same log.
    ThreadCreated,
    /// A client input was accepted. Durable BEFORE the acknowledgement, so a
    /// client holding a `TurnId` knows the input survived a crash.
    InputSubmitted {
        turn_id: TurnId,
        /// Client-side idempotency key. Deduplication lands in US-009; the
        /// field exists now so the durable format does not change later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_message_id: Option<String>,
        text: String,
    },
    /// A turn changed state. `from` is absent only for the initial `queued`.
    TurnStateChanged {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<TurnState>,
        to: TurnState,
        /// Bounded cause of a terminal state (FR-04, observability NFR).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<String>,
        /// Configuration the turn was committed to. Carried by the transition
        /// that STARTS the turn and by no other, so a resumed thread can report
        /// what its last turn ran under without a second event per turn
        /// (US-009 AC1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<TurnContext>,
    },
    /// A branch was materialized at a terminal turn boundary (US-010).
    Forked {
        child_thread_id: ThreadId,
        fork_turn_id: TurnId,
        fork_event_id: EventId,
    },
    /// A parent-child agent relation was reserved (US-012).
    AgentLinked {
        agent_id: AgentId,
        child_thread_id: ThreadId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SequentialIds;

    #[test]
    fn an_event_round_trips_and_exposes_its_owning_turn() {
        let ids = SequentialIds::new();
        let turn_id = TurnId::generate(&ids);
        let event = ThreadEvent {
            event_id: EventId::generate(&ids),
            thread_id: ThreadId::generate(&ids),
            seq: 3,
            at_ms: 1_700_000_000_000,
            payload: ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains("\"kind\":\"turn_state_changed\""));
        // Absent options stay out of the durable line.
        assert!(!line.contains("cause"));
        assert!(!line.contains("context"));
        assert_eq!(serde_json::from_str::<ThreadEvent>(&line).unwrap(), event);
        assert_eq!(event.turn_id(), Some(turn_id));
    }

    /// The captured configuration survives the file (US-009 AC1), and a log
    /// written before it existed still replays: the field is optional.
    #[test]
    fn a_turn_configuration_round_trips_and_stays_optional() {
        use crate::context::{TurnContext, TurnLimits};

        let ids = SequentialIds::new();
        let turn_id = TurnId::generate(&ids);
        let event = ThreadEvent {
            event_id: EventId::generate(&ids),
            thread_id: ThreadId::generate(&ids),
            seq: 1,
            at_ms: 0,
            payload: ThreadEventPayload::TurnStateChanged {
                turn_id,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: Some(TurnContext {
                    turn_id,
                    model: "gpt-5.4-codex".into(),
                    reasoning_effort: Some("high".into()),
                    permission_mode: "ask".into(),
                    sandbox: "workspace-write".into(),
                    workspace: std::path::PathBuf::from("/home/arthur/dev/pyxis"),
                    limits: TurnLimits {
                        max_turns: 50,
                        max_output_tokens: 4096,
                        max_pending_inputs: 16,
                    },
                }),
            },
        };
        let line = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<ThreadEvent>(&line).unwrap(), event);

        let without = r#"{"event_id":"evt_00000000000000000000000000000009",
            "thread_id":"thr_00000000000000000000000000000008","seq":1,"at_ms":0,
            "payload":{"kind":"turn_state_changed",
            "turn_id":"trn_00000000000000000000000000000007","to":"running"}}"#;
        let older = serde_json::from_str::<ThreadEvent>(without).expect("a v1 line still replays");
        assert!(matches!(
            older.payload,
            ThreadEventPayload::TurnStateChanged {
                context: None,
                from: None,
                ..
            }
        ));
    }

    #[test]
    fn thread_scoped_events_have_no_owning_turn() {
        let ids = SequentialIds::new();
        let event = ThreadEvent {
            event_id: EventId::generate(&ids),
            thread_id: ThreadId::generate(&ids),
            seq: 1,
            at_ms: 0,
            payload: ThreadEventPayload::ThreadCreated,
        };
        assert_eq!(event.turn_id(), None);
    }

    #[test]
    fn an_event_carrying_a_foreign_identifier_is_refused() {
        let raw = r#"{"event_id":"trn_00000000000000000000000000000001",
            "thread_id":"thr_00000000000000000000000000000002","seq":1,"at_ms":0,
            "payload":{"kind":"thread_created"}}"#;
        assert!(serde_json::from_str::<ThreadEvent>(raw).is_err());
    }
}
