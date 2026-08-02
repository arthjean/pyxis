//! Bounded Responses WebSocket session shared by Responses providers.
//!
//! A single session task owns the socket. Callers submit one bounded command at
//! a time, so connection ownership, cancellation, close handshakes, fallback,
//! and continuation state all have one canonical control flow.

use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll};

use agent_auth::provider::ProviderRequestAuth;
use agent_core::provider::{CanonicalRequest, ProviderError, StreamEvent};
use futures_util::Stream;
use futures_util::stream::BoxStream;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::chatgpt_http::{PreparedResponsesRoute, ResponsesTransportConfig};

mod continuation;
mod probe;
mod session;
mod transport;

pub use probe::{WebSocketProbeAuthorization, WebSocketProbeReport, WebSocketProbeVerdict};
use session::{PreconnectRequest, SessionCommand, SessionOutcome, StreamRequest, TurnScope};
use transport::{WebSocketPolicy, validate_message_bytes};

const COMMAND_BUFFER: usize = 1;
const EVENT_BUFFER: usize = 32;

pub(crate) enum WebSocketOutcome {
    Stream(BoxStream<'static, Result<StreamEvent, ProviderError>>),
    FallbackHttp,
}

pub(crate) struct WebSocketExecution<'a> {
    pub(crate) auth: &'a ProviderRequestAuth,
    pub(crate) config: &'a ResponsesTransportConfig,
    pub(crate) request: &'a CanonicalRequest,
    pub(crate) route: &'a PreparedResponsesRoute,
    pub(crate) full_body: Value,
    pub(crate) body_bytes: &'a [u8],
    pub(crate) provider_attempts: u32,
}

pub(crate) struct WebSocketProbeExecution<'a> {
    pub(crate) authorization: WebSocketProbeAuthorization,
    pub(crate) auth: &'a ProviderRequestAuth,
    pub(crate) config: &'a ResponsesTransportConfig,
    pub(crate) request: &'a CanonicalRequest,
    pub(crate) route: &'a PreparedResponsesRoute,
    pub(crate) full_body: Value,
    pub(crate) body_bytes: &'a [u8],
}

struct ScopeState {
    generation: u64,
    cancelled: CancellationToken,
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            generation: 1,
            cancelled: CancellationToken::new(),
        }
    }
}

struct ScopeSnapshot {
    generation: u64,
    cancelled: CancellationToken,
}

/// Lazily starts one actor because providers are also constructed in sync test
/// and discovery paths where no Tokio runtime exists yet.
pub(crate) struct ResponsesWebSocket {
    actor: Mutex<Option<mpsc::Sender<SessionCommand>>>,
    scope: StdMutex<ScopeState>,
}

impl ResponsesWebSocket {
    pub(crate) fn new() -> Self {
        Self {
            actor: Mutex::new(None),
            scope: StdMutex::new(ScopeState::default()),
        }
    }

    /// Invalidates every transport-local value synchronously. An active command
    /// observes cancellation immediately; an idle connection is closed before
    /// the next command after the generation change.
    pub(crate) fn reset_scope(&self) {
        let mut scope = match self.scope.lock() {
            Ok(scope) => scope,
            Err(poisoned) => poisoned.into_inner(),
        };
        scope.cancelled.cancel();
        scope.generation = scope.generation.saturating_add(1);
        scope.cancelled = CancellationToken::new();
    }

    pub(crate) async fn disconnect(&self, config: &ResponsesTransportConfig) {
        self.reset_scope();
        let sender = self.actor.lock().await.clone();
        let Some(sender) = sender else {
            return;
        };
        let (reply, completed) = oneshot::channel();
        if sender
            .send(SessionCommand::Disconnect {
                policy: WebSocketPolicy::from_config(config),
                completed: reply,
            })
            .await
            .is_ok()
        {
            let _ = completed.await;
        }
    }

    pub(crate) async fn preconnect(
        &self,
        auth: &ProviderRequestAuth,
        config: &ResponsesTransportConfig,
        request: &CanonicalRequest,
        route: &PreparedResponsesRoute,
    ) -> Result<(), ProviderError> {
        let prepared = config.prepare_websocket(route, auth)?;
        let scope = self.scope_snapshot();
        let mut cancel_guard = CancelOnDrop::new(scope.cancelled.clone());
        let (ready, response) = oneshot::channel();
        self.sender()
            .await
            .send(SessionCommand::Preconnect(PreconnectRequest {
                generation: scope.generation,
                turn: TurnScope::from_request(request),
                prepared,
                policy: WebSocketPolicy::from_config(config),
                cancelled: scope.cancelled,
                ready,
            }))
            .await
            .map_err(|_| ProviderError::Transport("websocket session task stopped".into()))?;
        let result = response
            .await
            .map_err(|_| ProviderError::Transport("websocket session task stopped".into()))?;
        cancel_guard.disarm();
        result
    }

    pub(crate) async fn stream(
        &self,
        execution: WebSocketExecution<'_>,
    ) -> Result<WebSocketOutcome, ProviderError> {
        let WebSocketExecution {
            auth,
            config,
            request,
            route,
            full_body,
            body_bytes,
            provider_attempts,
        } = execution;
        // Validate every non-secret component before reading credentials.
        validate_message_bytes(body_bytes)?;
        let prepared = config.prepare_websocket(route, auth)?;
        let scope = self.scope_snapshot();
        let mut cancel_guard = CancelOnDrop::new(scope.cancelled.clone());
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER);
        let (ready, response) = oneshot::channel();
        self.sender()
            .await
            .send(SessionCommand::Stream(StreamRequest {
                generation: scope.generation,
                turn: TurnScope::from_request(request),
                prepared,
                policy: WebSocketPolicy::from_config(config),
                full_body,
                replay_reasoning: request.reasoning_replay,
                provider_attempts,
                cancelled: scope.cancelled.clone(),
                events: events_tx,
                ready,
            }))
            .await
            .map_err(|_| ProviderError::Transport("websocket session task stopped".into()))?;
        let outcome = response
            .await
            .map_err(|_| ProviderError::Transport("websocket session task stopped".into()))??;
        cancel_guard.disarm();
        Ok(match outcome {
            SessionOutcome::Streaming => WebSocketOutcome::Stream(Box::pin(CancelOnDropStream {
                receiver: events_rx,
                cancelled: scope.cancelled,
            })),
            SessionOutcome::FallbackHttp => WebSocketOutcome::FallbackHttp,
        })
    }

    pub(crate) async fn probe(
        &self,
        execution: WebSocketProbeExecution<'_>,
    ) -> WebSocketProbeReport {
        probe::run(execution).await
    }

    async fn sender(&self) -> mpsc::Sender<SessionCommand> {
        let mut actor = self.actor.lock().await;
        if let Some(sender) = actor.as_ref().filter(|sender| !sender.is_closed()) {
            return sender.clone();
        }
        let (sender, receiver) = mpsc::channel(COMMAND_BUFFER);
        tokio::spawn(session::run(receiver));
        *actor = Some(sender.clone());
        sender
    }

    fn scope_snapshot(&self) -> ScopeSnapshot {
        let scope = match self.scope.lock() {
            Ok(scope) => scope,
            Err(poisoned) => poisoned.into_inner(),
        };
        ScopeSnapshot {
            generation: scope.generation,
            cancelled: scope.cancelled.child_token(),
        }
    }
}

struct CancelOnDrop {
    cancelled: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancelled: CancellationToken) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.cancel();
        }
    }
}

struct CancelOnDropStream {
    receiver: mpsc::Receiver<Result<StreamEvent, ProviderError>>,
    cancelled: CancellationToken,
}

impl Stream for CancelOnDropStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl Drop for CancelOnDropStream {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

#[cfg(test)]
mod tests;
