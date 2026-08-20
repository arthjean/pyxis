//! `agent-core`: the headless core of Pyxis. State-machine agent loop,
//! canonical types, context budget, compaction. Emits ONLY
//! `AgentEvent` (never ANSI). Testable without a real API/terminal/disk
//! (injectable deps). Invariants: see ARCHITECTURE.md "Invariants never to
//! violate".
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod auxiliary;
pub mod budget;
pub mod cancel;
pub mod clock;
pub mod compaction;
pub mod deps;
pub mod error;
pub mod event;
pub mod guardrail;
pub mod input;
pub mod message;
pub mod model;
pub mod permission;
pub mod prompt;
pub mod provider;
mod provider_extension;
pub mod quota;
pub mod redaction;
mod request;
mod response_item;
mod response_metadata;
pub mod sandbox;
pub mod session;
pub mod step;
pub mod sync;
mod tool_spec;
pub mod tools;
pub mod transition;

pub use agent::{
    AgentContext, HeadlessEnd, HeadlessResult, RunConfig, run_agent, run_headless,
    run_headless_observed,
};
pub use auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase, AuxiliaryProvider};
pub use budget::ContextBudget;
pub use cancel::{Cancellable, CancellationToken};
pub use compaction::CompactKind;
pub use deps::Deps;
pub use error::{AgentError, ProviderFailure, ProviderFailureKind};
pub use event::{
    AgentEvent, CredentialRefreshOutcome, CredentialRefreshView, FileChange, FileDiffView,
    InterruptReason, InterruptedView, ModelTurnView, PlanStatus, PlanStep, PlanView,
    RetryScheduledView, TurnDiffView,
};
pub use guardrail::{
    CostBudget, LOOP_GUARD_ARGS_PREVIEW_BYTES, LOOP_GUARD_THRESHOLDS, LoopDecision, LoopGuard,
    LoopRegister, UsageBudget, loop_guard_message, loop_guard_thresholds_are_valid,
};
pub use input::InputQueue;
pub use message::{
    ContentBlock, INTERRUPTED_TOOL_RESULT, Message, Role, ToolErrorKind, unanswered_tool_calls,
};
pub use permission::PermissionMode;
pub use prompt::{
    ContextBaseline, ContextTransition, ContextTransitionCause, PromptSnapshot, PromptSnapshotError,
};
pub use provider::{
    AuthError, CacheCapabilities, Capabilities, CapabilityLimits, ErrorClass, Provider,
    ProviderError, ProviderKind, ReasoningCapabilities, StopReason, StreamEvent, TokenUsage,
    ToolCallingCapabilities,
};
pub use quota::{QuotaSnapshot, QuotaWindow, format_unix_utc};
pub use sandbox::{SandboxPolicy, WritableRoot, WriteRefusal};
pub use session::{Session, SessionEntry, SessionError};
pub use step::{ContextFragmentKind, StepContextSource, StepFrame};
pub use tools::{
    MAX_MODEL_TOOL_RESULT_BYTES, ModelToolResult, StepToolPlan, ToolDispatch, ToolDispatchEvent,
    ToolDispatchSnapshot, ToolEventSink, ToolExecution, ToolImage, ToolInvocation,
    ToolResultStatus, ToolResultTruncation, TruncationStrategy,
};
