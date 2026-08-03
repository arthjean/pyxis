//! `Deps`: all the I/O dependencies of the loop, injected as traits
//! (ARCHITECTURE 3.2). This is what makes `run_agent` testable without a real
//! API, without a terminal, without a real disk (EP-002 DoD).

use std::sync::Arc;

use agent_tokenizer::TokenCounter;

use crate::cancel::CancellationToken;
use crate::clock::Clock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::ToolDispatch;

#[derive(Clone)]
pub struct Deps {
    pub provider: Arc<dyn Provider>,
    pub session: Arc<dyn Session>,
    pub tokenizer: Arc<dyn TokenCounter>,
    pub clock: Arc<dyn Clock>,
    pub tools: Arc<dyn ToolDispatch>,
    /// US-001: cooperative cancellation signal. A token never signalled leaves
    /// the loop behavior strictly unchanged.
    ///
    /// US-008: this is a node of the ONE process-wide cancellation tree. Callers
    /// pass the child token of the turn they are running, never a fresh root:
    /// a fresh root is an orphan branch that no interruption reaches.
    pub cancel: CancellationToken,
    /// Where the loop publishes the state of its context budget, for readers
    /// outside it (the `get_context_remaining` tool, a client that displays the
    /// pressure). Publishing into a handle nobody kept is a no-op, so the
    /// default costs a write to an `Arc` per turn and changes no behavior.
    pub context_window: crate::budget::ContextWindowState,
}
