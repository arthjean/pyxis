//! `agent-runtime`: local durable thread runtime (ADR-12, EP-001).
//!
//! Sits between `agent-core` (the turn engine) and the clients. It owns what
//! `run_agent` deliberately does not: durable identity, the control mailbox, the
//! turn lifecycle, the persistence of orchestration operations, the hierarchical
//! cancellation tree and the shutdown.
//!
//! Boundaries:
//! - `run_agent` stays the ONLY model-tools loop. The runtime never
//!   re-implements retry, compaction or dispatch.
//! - No disk access here: the local adapter of the orchestration store lives in
//!   `agent-session`, which depends on this crate and never the other way round.
//! - No public configuration key. Every v1 limit is a constant (FR-20).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod event;
pub mod id;
pub mod lifecycle;
pub mod store;

pub use event::{
    THREAD_EVENT_ENTRY, THREAD_META_ENTRY, THREAD_RUNTIME_VERSION, ThreadEvent, ThreadEventPayload,
};
pub use id::{
    AgentId, EventId, IdError, IdGenerator, RandomIds, SequentialIds, StepId, ThreadId, TurnId,
};
pub use lifecycle::{LifecycleError, TurnLifecycle, TurnState};
pub use store::{MemoryThreadStore, StoreError, ThreadSnapshot, ThreadStore};
