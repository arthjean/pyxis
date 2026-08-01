//! Single-owner WebSocket session actor.

use agent_core::provider::{
    CanonicalRequest, ProviderError, ProviderErrorCategory, StreamEvent, TURN_ID_METADATA_KEY,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::chatgpt_error::terminal_error;
use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_http::PreparedWebSocketRequest;

use super::continuation::{
    ContinuationInput, ContinuationState, GenerationMode, RequestMode, ResponseCapture,
    capture_response_state, prepare_wire_body, previous_response_not_found, response_create_body,
    validate_turn_state,
};
use super::transport::{
    Connection, WebSocketPolicy, close_connection, connect, http_status, is_auth_error,
    map_websocket_error, send_text, validate_message_bytes,
};

const SESSION_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TurnScope {
    Scoped(String),
    Unscoped,
}

impl TurnScope {
    pub(super) fn from_request(request: &CanonicalRequest) -> Self {
        request
            .client_metadata
            .get(TURN_ID_METADATA_KEY)
            .filter(|turn_id| !turn_id.is_empty())
            .cloned()
            .map_or(Self::Unscoped, Self::Scoped)
    }
}

pub(super) struct PreconnectRequest {
    pub(super) generation: u64,
    pub(super) turn: TurnScope,
    pub(super) prepared: PreparedWebSocketRequest,
    pub(super) policy: WebSocketPolicy,
    pub(super) cancelled: CancellationToken,
    pub(super) ready: oneshot::Sender<Result<(), ProviderError>>,
}

pub(super) struct StreamRequest {
    pub(super) generation: u64,
    pub(super) turn: TurnScope,
    pub(super) prepared: PreparedWebSocketRequest,
    pub(super) policy: WebSocketPolicy,
    pub(super) full_body: Value,
    pub(super) replay_reasoning: bool,
    pub(super) provider_attempts: u32,
    pub(super) cancelled: CancellationToken,
    pub(super) events: mpsc::Sender<Result<StreamEvent, ProviderError>>,
    pub(super) ready: oneshot::Sender<Result<SessionOutcome, ProviderError>>,
}

pub(super) enum SessionCommand {
    Preconnect(PreconnectRequest),
    Stream(StreamRequest),
    Disconnect {
        policy: WebSocketPolicy,
        completed: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionOutcome {
    Streaming,
    FallbackHttp,
}

struct SessionState {
    generation: u64,
    turn_id: Option<String>,
    turn_state: Option<String>,
    connection: Option<Connection>,
    continuation: Option<ContinuationState>,
    consecutive_failures: u32,
    failure_budget: u32,
    fallback_http: bool,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            generation: 0,
            turn_id: None,
            turn_state: None,
            connection: None,
            continuation: None,
            consecutive_failures: 0,
            failure_budget: 1,
            fallback_http: false,
        }
    }
}

pub(super) async fn run(mut commands: mpsc::Receiver<SessionCommand>) {
    let mut state = SessionState::default();
    while let Some(command) = commands.recv().await {
        match command {
            SessionCommand::Preconnect(request) => preconnect(&mut state, request).await,
            SessionCommand::Stream(request) => stream(&mut state, request).await,
            SessionCommand::Disconnect { policy, completed } => {
                close_connection(&mut state.connection, policy.close_timeout).await;
                state = SessionState::default();
                let _ = completed.send(());
            }
        }
    }
    close_connection(&mut state.connection, SESSION_SHUTDOWN_TIMEOUT).await;
}

async fn preconnect(state: &mut SessionState, request: PreconnectRequest) {
    synchronize_scope(state, request.generation, request.policy).await;
    synchronize_turn(state, request.turn);
    renew_expired_connection(state, request.policy.close_timeout).await;
    if state.fallback_http || state.connection.is_some() {
        let _ = request.ready.send(Ok(()));
        return;
    }
    let result = tokio::select! {
        biased;
        () = request.cancelled.cancelled() => Err(cancelled_error()),
        result = connect(request.prepared, request.policy) => result,
    };
    match result {
        Ok(connection) => {
            state.connection = Some(connection);
            let _ = request.ready.send(Ok(()));
        }
        Err(error) if http_status(&error) == Some(426) => {
            state.fallback_http = true;
            let _ = request.ready.send(Ok(()));
        }
        Err(error) => {
            let _ = request.ready.send(Err(error));
        }
    }
}

async fn stream(state: &mut SessionState, request: StreamRequest) {
    synchronize_scope(state, request.generation, request.policy).await;
    synchronize_turn(state, request.turn);
    renew_expired_connection(state, request.policy.close_timeout).await;
    state.failure_budget = request.provider_attempts.saturating_sub(1).max(1);
    if state.fallback_http {
        let _ = request.ready.send(Ok(SessionOutcome::FallbackHttp));
        return;
    }

    if state.connection.is_none() {
        let connected = tokio::select! {
            biased;
            () = request.cancelled.cancelled() => Err(cancelled_error()),
            result = connect(request.prepared, request.policy) => result,
        };
        match connected {
            Ok(connection) => state.connection = Some(connection),
            Err(error) if http_status(&error) == Some(426) => {
                state.fallback_http = true;
                let _ = request.ready.send(Ok(SessionOutcome::FallbackHttp));
                return;
            }
            Err(error) if is_auth_error(&error) => {
                let _ = request.ready.send(Err(error));
                return;
            }
            Err(error) => {
                if record_transport_failure(state) {
                    let _ = request.ready.send(Ok(SessionOutcome::FallbackHttp));
                } else {
                    let _ = request.ready.send(Err(error));
                }
                return;
            }
        }
    }

    if state.turn_state.is_none() {
        let handshake_turn_state = state
            .connection
            .as_ref()
            .and_then(Connection::handshake_metadata)
            .and_then(|metadata| metadata.turn_state.clone());
        if let Some(handshake_turn_state) = handshake_turn_state {
            match validate_turn_state(&handshake_turn_state) {
                Ok(turn_state) => state.turn_state = Some(turn_state),
                Err(error) => {
                    close_session_connection(state, request.policy.close_timeout).await;
                    let _ = request.ready.send(Err(error));
                    return;
                }
            }
        }
    }
    let (wire_body, mode) = prepare_wire_body(
        &request.full_body,
        state.continuation.as_ref(),
        state.turn_state.as_deref(),
    );
    let request_text = match serialize_request(&wire_body) {
        Ok(text) => text,
        Err(error) => {
            let _ = request.ready.send(Err(error));
            return;
        }
    };
    let sent = {
        let Some(connection) = state.connection.as_mut() else {
            let _ = request.ready.send(Err(ProviderError::Stream(
                "websocket connection is unavailable".into(),
            )));
            return;
        };
        tokio::select! {
            biased;
            () = request.cancelled.cancelled() => Err(cancelled_error()),
            result = send_text(&mut connection.socket, request_text, request.policy.write_timeout) => result,
        }
    };
    if let Err(error) = sent {
        close_session_connection(state, request.policy.close_timeout).await;
        if request.cancelled.is_cancelled() {
            let _ = request.ready.send(Err(cancelled_error()));
        } else {
            record_transport_failure(state);
            let _ = request.ready.send(Err(outcome_unknown(&error)));
        }
        return;
    }

    if request.ready.send(Ok(SessionOutcome::Streaming)).is_err() {
        close_session_connection(state, request.policy.close_timeout).await;
        return;
    }
    drive_response(
        state,
        request.events,
        request.cancelled,
        request.full_body,
        mode,
        request.replay_reasoning,
        request.policy,
    )
    .await;
}

async fn drive_response(
    state: &mut SessionState,
    events: mpsc::Sender<Result<StreamEvent, ProviderError>>,
    cancelled: CancellationToken,
    full_body: Value,
    mode: RequestMode,
    replay_reasoning: bool,
    policy: WebSocketPolicy,
) {
    let mut mapper = CodexEventMapper::with_replay(replay_reasoning);
    let mut capture = ResponseCapture::default();
    let mut retried_full = false;
    let mut published_event = false;

    if let Some(connection) = state.connection.as_mut() {
        for event in connection.take_initial_events() {
            if !publish_event(&events, &cancelled, event).await {
                close_session_connection(state, policy.close_timeout).await;
                return;
            }
        }
    }

    loop {
        let next = {
            let Some(connection) = state.connection.as_mut() else {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    ProviderError::Stream("websocket connection is unavailable".into()),
                    policy.close_timeout,
                )
                .await;
                return;
            };
            tokio::select! {
                biased;
                () = cancelled.cancelled() => {
                    close_session_connection(state, policy.close_timeout).await;
                    return;
                }
                message = tokio::time::timeout(policy.idle_timeout, connection.socket.next()) => message,
            }
        };

        let message = match next {
            Err(_) => {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    ProviderError::Stream("websocket idle timeout".into()),
                    policy.close_timeout,
                )
                .await;
                return;
            }
            Ok(None) => {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    ProviderError::Stream("websocket closed before terminal".into()),
                    policy.close_timeout,
                )
                .await;
                return;
            }
            Ok(Some(Err(error))) => {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    map_websocket_error(error),
                    policy.close_timeout,
                )
                .await;
                return;
            }
            Ok(Some(Ok(message))) => message,
        };

        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Ping(payload) => {
                let pong = match state.connection.as_mut() {
                    Some(connection) => tokio::select! {
                        biased;
                        () = cancelled.cancelled() => None,
                        result = tokio::time::timeout(
                            policy.write_timeout,
                            connection.socket.send(Message::Pong(payload)),
                        ) => Some(result),
                    },
                    None => continue,
                };
                let Some(pong) = pong else {
                    close_session_connection(state, policy.close_timeout).await;
                    return;
                };
                if !matches!(pong, Ok(Ok(()))) {
                    fail_unknown(
                        state,
                        &events,
                        &cancelled,
                        ProviderError::Stream("websocket pong failed".into()),
                        policy.close_timeout,
                    )
                    .await;
                    return;
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Binary(_) => {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    ProviderError::Decode("unexpected binary websocket frame".into()),
                    policy.close_timeout,
                )
                .await;
                return;
            }
            Message::Close(_) => {
                fail_unknown(
                    state,
                    &events,
                    &cancelled,
                    ProviderError::Stream("websocket closed before terminal".into()),
                    policy.close_timeout,
                )
                .await;
                return;
            }
            Message::Frame(_) => continue,
        };

        let raw: Option<Value> = serde_json::from_str(&text).ok();
        if let Some(raw) = &raw
            && let Err(error) = capture_response_state(raw, &mut capture, &mut state.turn_state)
        {
            fail_unknown(state, &events, &cancelled, error, policy.close_timeout).await;
            return;
        }
        if mode.is_incremental()
            && let Some(baseline_error) = raw
                .as_ref()
                .filter(|event| previous_response_not_found(event))
        {
            if retried_full || published_event {
                let error = terminal_error(
                    ProviderErrorCategory::Failed,
                    None,
                    "incremental baseline disappeared after response dispatch; replay refused",
                    baseline_error,
                );
                state.consecutive_failures = 0;
                close_session_connection(state, policy.close_timeout).await;
                send_terminal_error(&events, &cancelled, error).await;
                return;
            }
            let full_request_text = match serialize_request(&response_create_body(
                &full_body,
                ContinuationInput::Full,
                state.turn_state.as_deref(),
                GenerationMode::Generate,
            )) {
                Ok(text) => text,
                Err(error) => {
                    fail_unknown(state, &events, &cancelled, error, policy.close_timeout).await;
                    return;
                }
            };
            let resent = match state.connection.as_mut() {
                Some(connection) => tokio::select! {
                    biased;
                    () = cancelled.cancelled() => Err(cancelled_error()),
                    result = send_text(
                        &mut connection.socket,
                        full_request_text,
                        policy.write_timeout,
                    ) => result,
                },
                None => Err(ProviderError::Stream(
                    "websocket connection is unavailable".into(),
                )),
            };
            if let Err(error) = resent {
                if cancelled.is_cancelled() {
                    close_session_connection(state, policy.close_timeout).await;
                    return;
                }
                fail_unknown(state, &events, &cancelled, error, policy.close_timeout).await;
                return;
            }
            state.continuation = None;
            mapper = CodexEventMapper::with_replay(replay_reasoning);
            capture = ResponseCapture::default();
            retried_full = true;
            continue;
        }

        let mapped = mapper.ingest(&text);
        let mapped = match mapped {
            Ok(mapped) => mapped,
            Err(error @ ProviderError::Api { .. }) => {
                state.consecutive_failures = 0;
                close_session_connection(state, policy.close_timeout).await;
                send_terminal_error(&events, &cancelled, error).await;
                return;
            }
            Err(error) => {
                fail_unknown(state, &events, &cancelled, error, policy.close_timeout).await;
                return;
            }
        };
        for event in mapped {
            let terminal = matches!(event, StreamEvent::Done { .. });
            published_event = true;
            if !publish_event(&events, &cancelled, event).await {
                close_session_connection(state, policy.close_timeout).await;
                return;
            }
            if terminal {
                state.continuation = capture.into_continuation(&full_body);
                state.consecutive_failures = 0;
                return;
            }
        }
    }
}

async fn renew_expired_connection(state: &mut SessionState, close_timeout: std::time::Duration) {
    if state
        .connection
        .as_ref()
        .is_some_and(Connection::renewal_due)
    {
        close_session_connection(state, close_timeout).await;
    }
}

async fn synchronize_scope(state: &mut SessionState, generation: u64, policy: WebSocketPolicy) {
    if state.generation == generation {
        return;
    }
    close_connection(&mut state.connection, policy.close_timeout).await;
    *state = SessionState {
        generation,
        ..SessionState::default()
    };
}

fn synchronize_turn(state: &mut SessionState, turn: TurnScope) {
    let next = match turn {
        TurnScope::Scoped(turn_id) => Some(turn_id),
        TurnScope::Unscoped => None,
    };
    let reusable = next.is_some() && state.turn_id == next;
    if reusable {
        return;
    }
    state.turn_id = next;
    state.turn_state = None;
    state.continuation = None;
}

fn record_transport_failure(state: &mut SessionState) -> bool {
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    if state.consecutive_failures >= state.failure_budget {
        state.fallback_http = true;
    }
    state.fallback_http
}

fn serialize_request(body: &Value) -> Result<String, ProviderError> {
    let text = serde_json::to_string(body)
        .map_err(|_| ProviderError::Decode("websocket request serialization failed".into()))?;
    validate_message_bytes(text.as_bytes())?;
    Ok(text)
}

fn cancelled_error() -> ProviderError {
    ProviderError::Stream("websocket operation cancelled".into())
}

fn outcome_unknown(cause: &ProviderError) -> ProviderError {
    terminal_error(
        ProviderErrorCategory::Failed,
        None,
        &format!("websocket request outcome unknown; replay refused: {cause}"),
        &Value::Null,
    )
}

async fn fail_unknown(
    state: &mut SessionState,
    events: &mpsc::Sender<Result<StreamEvent, ProviderError>>,
    cancelled: &CancellationToken,
    cause: ProviderError,
    close_timeout: std::time::Duration,
) {
    record_transport_failure(state);
    close_session_connection(state, close_timeout).await;
    send_terminal_error(events, cancelled, outcome_unknown(&cause)).await;
}

async fn send_terminal_error(
    events: &mpsc::Sender<Result<StreamEvent, ProviderError>>,
    cancelled: &CancellationToken,
    error: ProviderError,
) {
    tokio::select! {
        biased;
        () = cancelled.cancelled() => {}
        _ = events.send(Err(error)) => {}
    }
}

async fn publish_event(
    events: &mpsc::Sender<Result<StreamEvent, ProviderError>>,
    cancelled: &CancellationToken,
    event: StreamEvent,
) -> bool {
    tokio::select! {
        biased;
        () = cancelled.cancelled() => false,
        result = events.send(Ok(event)) => result.is_ok(),
    }
}

async fn close_session_connection(state: &mut SessionState, close_timeout: std::time::Duration) {
    close_connection(&mut state.connection, close_timeout).await;
    state.turn_state = None;
    state.continuation = None;
}

#[cfg(test)]
mod tests;
