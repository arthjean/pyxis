//! `agent-runtime`: local durable thread runtime (ADR-12, EP-001).
//!
//! Sits between `agent-core` (the turn engine) and the clients. It owns what
//! `run_agent` deliberately does not: durable identity, the control mailbox, the
//! turn lifecycle, the persistence of orchestration operations, the hierarchical
//! cancellation tree and the shutdown.
//!
//! Boundaries:
//! - `run_agent` stays the ONLY model-tools loop. The runtime reaches it through
//!   [`runner::TurnRunner`] and never re-implements retry, compaction or
//!   dispatch.
//! - No disk access here. [`store::ThreadStore`] is a trait; its JSONL adapter
//!   lives in `agent-session`, its in-memory adapter in [`store`].
//! - No public configuration key. Every v1 limit is a constant of
//!   [`thread`] (FR-20).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod context;
pub mod event;
pub mod id;
pub mod inputs;
pub mod lifecycle;
pub mod runner;
pub mod store;
pub mod thread;

pub use context::{
    FixedTurnContext, MAX_SECTION_BYTES, MAX_STEP_CONTEXT_BYTES, StepContext, StepContexts,
    StepSection, StepSnapshot, StepSource, TurnContext, TurnContextSource, TurnLimits,
};
pub use event::{
    THREAD_EVENT_ENTRY, THREAD_META_ENTRY, THREAD_RUNTIME_VERSION, ThreadEvent, ThreadEventPayload,
};
pub use id::{
    AgentId, EventId, IdError, IdGenerator, RandomIds, SequentialIds, StepId, ThreadId, TurnId,
};
pub use inputs::TurnInputs;
pub use lifecycle::{LifecycleError, TurnLifecycle, TurnState};
pub use runner::{RunAgentRunner, TurnOutcome, TurnRequest, TurnRunner};
pub use store::{MemoryThreadStore, StoreError, ThreadSnapshot, ThreadStore};
pub use thread::{
    Accepted, COMMAND_MAILBOX, LIVE_EVENT_BUFFER, MAX_PENDING_INPUTS, RuntimeError, RuntimeEvent,
    RuntimeEventPayload, SHUTDOWN_DEADLINE, STRAGGLER_ABORT_AFTER, Submission, SubmitError,
    ThreadHandle, ThreadOptions, ThreadStatus, TurnStatus,
};
