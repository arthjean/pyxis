//! WebSocket handshake, bounded framing, and close-handshake primitives.

use std::time::{Duration, Instant};

use agent_core::provider::{ProviderError, ResponseMetadata, StreamEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::chatgpt_error::{from_http_parts, invalid_request};
use crate::chatgpt_http::{PreparedWebSocketRequest, ResponsesTransportConfig};
use crate::chatgpt_metadata::response_metadata_from_headers;

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const CONNECTION_RENEWAL_AGE: Duration = Duration::from_secs(55 * 60);

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Copy)]
pub(super) struct WebSocketPolicy {
    pub(super) connect_timeout: Duration,
    pub(super) write_timeout: Duration,
    pub(super) close_timeout: Duration,
    pub(super) idle_timeout: Duration,
    pub(super) max_write_buffer: usize,
}

impl WebSocketPolicy {
    pub(super) fn from_config(config: &ResponsesTransportConfig) -> Self {
        Self {
            connect_timeout: config.websocket_connect_timeout(),
            write_timeout: config.websocket_write_timeout(),
            close_timeout: config.websocket_close_timeout(),
            idle_timeout: config.idle_timeout(),
            max_write_buffer: config.websocket_write_buffer(),
        }
    }
}

pub(super) struct Connection {
    pub(super) socket: Socket,
    opened_at: Instant,
    handshake_metadata: Option<ResponseMetadata>,
    handshake_quotas: Vec<agent_core::quota::QuotaSnapshot>,
}

impl Connection {
    pub(super) fn renewal_due(&self) -> bool {
        renewal_due_at(self.opened_at, Instant::now())
    }

    pub(super) fn handshake_metadata(&self) -> Option<&ResponseMetadata> {
        self.handshake_metadata.as_ref()
    }

    pub(super) fn take_initial_events(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::with_capacity(self.handshake_quotas.len().saturating_add(1));
        events.extend(
            std::mem::take(&mut self.handshake_quotas)
                .into_iter()
                .map(|snapshot| StreamEvent::Quota { snapshot }),
        );
        if let Some(metadata) = self.handshake_metadata.take() {
            events.push(StreamEvent::ResponseMetadata {
                metadata: Box::new(metadata),
            });
        }
        events
    }
}

pub(super) async fn connect(
    prepared: PreparedWebSocketRequest,
    policy: WebSocketPolicy,
) -> Result<Connection, ProviderError> {
    let mut request = prepared
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| ProviderError::Transport("invalid websocket request".into()))?;
    request.headers_mut().extend(prepared.headers);
    let websocket_config = bounded_websocket_config(policy);
    let (socket, response) = tokio::time::timeout(
        policy.connect_timeout,
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false),
    )
    .await
    .map_err(|_| ProviderError::Stream("websocket connect timeout".into()))?
    .map_err(map_websocket_error)?;
    let metadata = response_metadata_from_headers(response.headers());
    let quotas = crate::quota::parse_all_quota_headers(response.headers());
    Ok(Connection {
        socket,
        opened_at: Instant::now(),
        handshake_metadata: (!metadata.is_empty()).then_some(metadata),
        handshake_quotas: quotas,
    })
}

fn renewal_due_at(opened_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(opened_at) >= CONNECTION_RENEWAL_AGE
}

pub(super) fn validate_message_bytes(bytes: &[u8]) -> Result<(), ProviderError> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(invalid_request("websocket request exceeds 64 MiB"));
    }
    Ok(())
}

pub(super) async fn send_text(
    socket: &mut Socket,
    text: String,
    timeout: Duration,
) -> Result<(), ProviderError> {
    validate_message_bytes(text.as_bytes())?;
    tokio::time::timeout(timeout, socket.send(Message::Text(text.into())))
        .await
        .map_err(|_| ProviderError::Stream("websocket write timeout".into()))?
        .map_err(map_websocket_error)
}

pub(super) async fn close_connection(connection: &mut Option<Connection>, timeout: Duration) {
    if let Some(mut connection) = connection.take() {
        let _ = close_socket(&mut connection.socket, timeout).await;
    }
}

pub(super) async fn close_socket(socket: &mut Socket, timeout: Duration) -> Option<u16> {
    tokio::time::timeout(timeout, async {
        if socket.close(None).await.is_err() {
            return None;
        }
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Close(frame)) => return frame.map(|frame| u16::from(frame.code)),
                Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => {
                    return Some(1000);
                }
                Err(_) => return None,
                _ => {}
            }
        }
        Some(1000)
    })
    .await
    .ok()
    .flatten()
}

pub(super) fn map_websocket_error(error: WebSocketError) -> ProviderError {
    match error {
        WebSocketError::Http(response) => {
            let body = response.body().as_deref().unwrap_or_default();
            let body = std::str::from_utf8(body).unwrap_or_default();
            from_http_parts(response.status().as_u16(), response.headers(), body)
        }
        WebSocketError::Capacity(error) => {
            ProviderError::Decode(format!("websocket capacity limit exceeded: {error}"))
        }
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
            ProviderError::Stream("websocket connection closed".into())
        }
        other => ProviderError::Transport(other.to_string()),
    }
}

pub(super) fn http_status(error: &ProviderError) -> Option<u16> {
    match error {
        ProviderError::Http { status, .. } => Some(*status),
        ProviderError::Api { status, .. } => *status,
        _ => None,
    }
}

pub(super) fn is_auth_error(error: &ProviderError) -> bool {
    matches!(http_status(error), Some(401 | 403))
        || matches!(
            error,
            ProviderError::Credential(_)
                | ProviderError::Api {
                    category: agent_core::provider::ProviderErrorCategory::Authentication
                        | agent_core::provider::ProviderErrorCategory::PermissionDenied,
                    ..
                }
        )
}

fn bounded_websocket_config(policy: WebSocketPolicy) -> WebSocketConfig {
    WebSocketConfig::default()
        .write_buffer_size(64 * 1024)
        .max_write_buffer_size(policy.max_write_buffer)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::ProviderErrorCategory;
    use tokio_tungstenite::tungstenite::http::Response;

    #[test]
    fn websocket_limits_are_explicit_and_bounded() {
        let config =
            ResponsesTransportConfig::new("https://chatgpt.com/backend-api/", "codex/responses")
                .expect("valid test config");
        let bounded = bounded_websocket_config(WebSocketPolicy::from_config(&config));
        assert_eq!(bounded.write_buffer_size, 64 * 1024);
        assert_eq!(bounded.max_write_buffer_size, 1024 * 1024);
        assert_eq!(bounded.max_message_size, Some(MAX_MESSAGE_BYTES));
        assert_eq!(bounded.max_frame_size, Some(MAX_FRAME_BYTES));
        assert!(CONNECTION_RENEWAL_AGE < Duration::from_secs(60 * 60));
        let now = Instant::now();
        assert!(!renewal_due_at(now, now));
        assert!(renewal_due_at(now, now + CONNECTION_RENEWAL_AGE));
    }

    #[test]
    fn upgrade_errors_keep_http_diagnostics_and_retry_delay() {
        let response = Response::builder()
            .status(429)
            .header("retry-after-ms", "1700")
            .header("x-request-id", "req_ws")
            .header("x-auth-request-id", "auth_ws")
            .body(Some(
                br#"{"error":{"code":"rate_limit_exceeded"}}"#.to_vec(),
            ))
            .expect("valid test response");
        let error = map_websocket_error(WebSocketError::Http(Box::new(response)));
        assert!(matches!(error, ProviderError::Api {
            category: ProviderErrorCategory::RateLimited,
            retry_after_ms: Some(1700),
            request_id: Some(ref request_id),
            auth_request_id: Some(ref auth_request_id),
            ..
        } if request_id == "req_ws" && auth_request_id == "auth_ws"));
    }
}
