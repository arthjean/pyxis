//! `Deps`: all the I/O dependencies of the loop, injected as traits
//! (ARCHITECTURE 3.2). This is what makes `run_agent` testable without a real
//! API, without a terminal, without a real disk (EP-002 DoD).

use std::sync::Arc;

use agent_tokenizer::TokenCounter;

use crate::cancel::CancelToken;
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
    /// US-001: cooperative cancellation signal. A token never signalled (default)
    /// leaves the loop behavior strictly unchanged.
    pub cancel: CancelToken,
}
