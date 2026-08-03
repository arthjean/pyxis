//! The seam between the protocol and the runtime assembly (US-016).
//!
//! The app-server owns the wire; it owns nothing of what it takes to build a
//! thread (a provider, a keyring, a sandbox, a tool registry). That half stays
//! in the binary and enters here through one trait. Two consequences, both
//! deliberate: the protocol is testable without a network or a credential, and
//! no orchestration decision the runtime already owns is re-decided here.

use std::sync::Arc;

use agent_runtime::thread::RuntimeEvent;
use tokio::sync::broadcast;

use crate::jsonrpc::{ErrorObject, error_code};
use crate::protocol::{DynamicToolSpec, ThreadItem};

/// Why the host refused. Each variant is a distinct JSON-RPC code: a client
/// retries a `Refused` and fixes an `UnknownThread`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("unknown thread `{0}`")]
    UnknownThread(String),
    #[error("the named turn is not running")]
    StaleTurn,
    /// The runtime declined: mailbox full, shutting down, store failure.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Internal(String),
}

impl HostError {
    pub fn into_error_object(self) -> ErrorObject {
        let code = match &self {
            Self::UnknownThread(_) => error_code::UNKNOWN_THREAD,
            Self::StaleTurn => error_code::UNKNOWN_TURN,
            Self::Refused(_) => error_code::RUNTIME_REFUSED,
            Self::Internal(_) => error_code::INTERNAL_ERROR,
        };
        ErrorObject::new(code, self.to_string())
    }
}

/// A thread the host opened for one connection.
pub struct OpenThread {
    pub thread_id: String,
    /// Items the durable log already held, in persistent order. Also seeds the
    /// live item numbering.
    pub items: Vec<ThreadItem>,
    /// Live runtime events. A `broadcast` because a stalled client must never
    /// block the thread actor.
    pub events: broadcast::Receiver<RuntimeEvent>,
    pub control: Arc<dyn ThreadControl>,
}

/// Write operations on one open thread.
#[async_trait::async_trait]
pub trait ThreadControl: Send + Sync {
    /// Opens a turn. Returns once the submission is durable.
    async fn submit(
        &self,
        text: String,
        client_message_id: Option<String>,
    ) -> Result<String, HostError>;

    /// Feeds the running turn. `expected_turn` is the client's claim about what
    /// it is correcting; naming a turn that ended is refused rather than turned
    /// into a new turn behind the user's back.
    async fn steer(
        &self,
        text: String,
        client_message_id: Option<String>,
        expected_turn: Option<String>,
    ) -> Result<String, HostError>;

    /// Signals the running turn. Acknowledged as soon as the cancellation is
    /// sent, never after the terminal state.
    async fn interrupt(&self, turn_id: Option<String>) -> Result<(), HostError>;

    /// Closes the thread: admission, cancellation, drain, store.
    async fn close(&self);
}

/// What the binary supplies so the protocol can drive real threads.
#[async_trait::async_trait]
pub trait RuntimeHost: Send + Sync + 'static {
    /// Opens a brand-new durable thread.
    async fn start_thread(
        &self,
        dynamic_tools: Vec<DynamicToolSpec>,
    ) -> Result<OpenThread, HostError>;

    /// Reopens the durable log of `thread_id`.
    async fn resume_thread(
        &self,
        thread_id: &str,
        dynamic_tools: Vec<DynamicToolSpec>,
    ) -> Result<OpenThread, HostError>;

    /// Persisted items of a thread, held open by this connection or not.
    ///
    /// Returns the WHOLE thread: the projection has to walk every message to
    /// number the items and to bind each result to the call it answers, so a
    /// range would not save the walk. `thread/items/list` therefore pages over
    /// a materialized list, and the cost of a page is the cost of the thread.
    /// A store that can project a range on its own is what would change that,
    /// and this signature is where it would be declared.
    async fn history(&self, thread_id: &str) -> Result<Vec<ThreadItem>, HostError>;

    /// Name and version published by `initialize`.
    fn server_name(&self) -> String {
        "pyxis".to_string()
    }

    fn server_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}
