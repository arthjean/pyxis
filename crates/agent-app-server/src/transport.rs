//! Transports (US-016, US-018 AC2).
//!
//! Both are thin on purpose: one protocol actor, two ways of moving its lines.
//! stdio is the default and needs no authorization, because a process that can
//! write to this process's standard input already runs as the user. The
//! WebSocket transport does not have that property, so it is loopback-only and
//! bearer-authorized, and it carries the SAME contract and the same identifiers
//! as stdio.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::{Message, http::StatusCode};

use crate::jsonrpc::{self, Outbound};
use crate::outbound::{Outbox, OutboxReceiver};
use crate::protocol::{ErrorNotification, ServerNotification};
use crate::server::{AppServer, Connection};

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("app-server io: {0}")]
    Io(#[from] std::io::Error),
    #[error("app-server listen address must be a loopback address, got {0}")]
    NotLoopback(SocketAddr),
    #[error("app-server websocket: {0}")]
    WebSocket(String),
}

/// Serves one client over standard input/output.
pub async fn serve_stdio(server: Arc<AppServer>) -> Result<(), ServeError> {
    let (outbox, receiver) = Outbox::new();
    let connection = Connection::new(server, outbox);
    let writer = tokio::spawn(write_lines(receiver, tokio::io::stdout()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if handle_line(&connection, &line).await.is_err() {
            break;
        }
    }
    connection.close_thread("the client disconnected").await;
    drop(connection);
    let _ = writer.await;
    Ok(())
}

/// Serves WebSocket clients on a loopback address.
///
/// `token` is required on every handshake: a loopback socket is reachable by
/// every process of the machine, so "local" is not an authorization.
pub async fn serve_websocket(
    server: Arc<AppServer>,
    addr: SocketAddr,
    token: Arc<String>,
) -> Result<(), ServeError> {
    if !is_loopback(&addr) {
        return Err(ServeError::NotLoopback(addr));
    }
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(
        target: "pyxis::app_server",
        addr = %listener.local_addr()?,
        "websocket transport listening"
    );
    loop {
        let (stream, peer) = listener.accept().await?;
        // A peer that is not loopback cannot reach a loopback listener, but the
        // check is repeated rather than assumed: it costs nothing and it is the
        // property the whole authorization rests on.
        if !is_loopback(&peer) {
            continue;
        }
        let server = Arc::clone(&server);
        let token = Arc::clone(&token);
        tokio::spawn(async move {
            if let Err(err) = serve_websocket_connection(server, stream, token).await {
                tracing::warn!(
                    target: "pyxis::app_server",
                    error = %err,
                    "websocket connection ended"
                );
            }
        });
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// A random bearer token for the WebSocket transport.
pub fn mint_token() -> String {
    use rand::RngCore;
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

async fn serve_websocket_connection(
    server: Arc<AppServer>,
    stream: TcpStream,
    token: Arc<String>,
) -> Result<(), ServeError> {
    let expected = Arc::clone(&token);
    let socket = tokio_tungstenite::accept_hdr_async(
        stream,
        // The `Err` type is the handshake response of the WebSocket library,
        // which is what its callback signature demands; boxing it is not ours
        // to decide.
        #[allow(clippy::result_large_err)]
        move |request: &HandshakeRequest, response: HandshakeResponse| {
            if authorized(request, &expected) {
                Ok(response)
            } else {
                let mut refusal = ErrorResponse::new(Some(
                    "missing or invalid bearer token for the app-server".to_string(),
                ));
                *refusal.status_mut() = StatusCode::UNAUTHORIZED;
                Err(refusal)
            }
        },
    )
    .await
    .map_err(|err| ServeError::WebSocket(err.to_string()))?;

    let (mut sink, mut source) = socket.split();
    let (outbox, receiver) = Outbox::new();
    let connection = Connection::new(server, outbox);
    let writer = tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            if sink
                .send(Message::Text(message.to_line().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(message) = source.next().await {
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                tracing::debug!(target: "pyxis::app_server", error = %err, "websocket read");
                break;
            }
        };
        let line = match message {
            Message::Text(text) => text,
            // A binary frame is legal WebSocket and is not this protocol.
            Message::Binary(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        if handle_line(&connection, &line).await.is_err() {
            break;
        }
    }
    connection.close_thread("the client disconnected").await;
    drop(connection);
    let _ = writer.await;
    Ok(())
}

fn authorized(request: &HandshakeRequest, expected: &str) -> bool {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    // Also accepted in the query string: a browser client cannot set a header
    // on a WebSocket handshake.
    let query = request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("token="))
        })
        .map(str::trim);
    matches!(header.or(query), Some(candidate) if constant_time_eq(candidate, expected))
}

/// Compares without an early exit on the first differing byte.
fn constant_time_eq(candidate: &str, expected: &str) -> bool {
    if candidate.len() != expected.len() {
        return false;
    }
    candidate
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Decodes one line, answers it, and reports whether the connection survives.
///
/// The only fatal outcome is a closed outbound queue: everything a client can
/// write, however broken, produces an answer and leaves the connection usable
/// (US-016 AC4).
pub async fn handle_line(connection: &Arc<Connection>, line: &str) -> Result<(), ()> {
    let answer = match jsonrpc::decode(line) {
        Ok(inbound) => connection.handle(inbound).await,
        Err((id, error)) => Some(Outbound::Error {
            id,
            error: error.into_error_object(),
        }),
    };
    if let Some(answer) = answer
        && let Err(overflow) = connection.outbox().send(answer)
    {
        report_overflow(connection, &overflow.detail);
        return Err(());
    }
    if let Some(overflow) = connection.outbox().overflow() {
        report_overflow(connection, &overflow.detail);
        return Err(());
    }
    Ok(())
}

/// Last message of a connection the server is closing. Written straight to the
/// queue: it is bounded, so this either fits or the client was already gone.
fn report_overflow(connection: &Arc<Connection>, detail: &str) {
    let _ = connection
        .outbox()
        .send(ServerNotification::Error(ErrorNotification {
            thread_id: None,
            turn_id: None,
            message: detail.to_string(),
        })
        .into());
    tracing::warn!(target: "pyxis::app_server", detail, "connection closed");
}

async fn write_lines<W: AsyncWrite + Unpin>(receiver: OutboxReceiver, mut writer: W) {
    while let Some(message) = receiver.recv().await {
        let mut line = message.to_line();
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_loopback_listener_is_refused() {
        assert!(is_loopback(&"127.0.0.1:0".parse().expect("addr")));
        assert!(is_loopback(&"[::1]:0".parse().expect("addr")));
        assert!(!is_loopback(&"0.0.0.0:0".parse().expect("addr")));
        assert!(!is_loopback(&"192.168.1.10:0".parse().expect("addr")));
    }

    #[test]
    fn tokens_are_compared_whole() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("ab", "abc"));
    }

    #[test]
    fn a_minted_token_is_long_and_never_repeats() {
        let first = mint_token();
        assert_eq!(first.len(), 64);
        assert_ne!(first, mint_token());
    }
}
