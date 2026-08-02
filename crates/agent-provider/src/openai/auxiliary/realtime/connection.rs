use std::collections::VecDeque;
use std::time::Duration;

use agent_core::auxiliary::realtime::{
    RealtimeAudioFrame, RealtimeContextAppendChannel, RealtimeEvent, RealtimeSessionConfig,
    RealtimeWire,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase, RealtimeSession};
use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::protocol::RealtimeCodec;
use super::session;

const MAX_REALTIME_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REALTIME_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Writer = SplitSink<Socket, Message>;
type Reader = SplitStream<Socket>;

pub(super) struct OpenAiRealtimeSession {
    writer: Mutex<Writer>,
    reader: Mutex<Reader>,
    pending: Mutex<VecDeque<RealtimeEvent>>,
    codec: RealtimeCodec,
    cancellation: CancellationToken,
    write_timeout: Duration,
    read_timeout: Duration,
    close_timeout: Duration,
}

impl std::fmt::Debug for OpenAiRealtimeSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiRealtimeSession")
            .field("codec", &self.codec)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl OpenAiRealtimeSession {
    pub(super) fn new(
        socket: Socket,
        codec: RealtimeCodec,
        cancellation: CancellationToken,
        write_timeout: Duration,
        read_timeout: Duration,
        close_timeout: Duration,
    ) -> Self {
        let (writer, reader) = socket.split();
        Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            pending: Mutex::new(VecDeque::new()),
            codec,
            cancellation,
            write_timeout,
            read_timeout,
            close_timeout,
        }
    }

    pub(super) async fn send_initial_session(
        &self,
        config: &RealtimeSessionConfig,
    ) -> Result<(), AuxiliaryError> {
        self.send_json(session::update_value(config)).await
    }

    pub(super) async fn confirm_frameless_started(&self) -> Result<(), AuxiliaryError> {
        let event = self
            .next_event()
            .await?
            .ok_or_else(|| AuxiliaryError::Decode {
                operation: AuxiliaryOperation::RealtimeWebSocket,
                phase: AuxiliaryPhase::Read,
                reason: "session ended before session.started".into(),
            })?;
        if !matches!(event, RealtimeEvent::SessionUpdated { .. }) {
            return Err(AuxiliaryError::Decode {
                operation: AuxiliaryOperation::RealtimeWebSocket,
                phase: AuxiliaryPhase::Read,
                reason: "event arrived before session.started".into(),
            });
        }
        self.pending.lock().await.push_back(event);
        Ok(())
    }

    async fn send_json(&self, value: serde_json::Value) -> Result<(), AuxiliaryError> {
        let payload = serde_json::to_string(&value).map_err(|_| AuxiliaryError::Decode {
            operation: AuxiliaryOperation::RealtimeWebSocket,
            phase: AuxiliaryPhase::Write,
            reason: "failed to encode realtime frame".into(),
        })?;
        if payload.len() > MAX_REALTIME_MESSAGE_BYTES {
            return Err(AuxiliaryError::invalid(
                AuxiliaryOperation::RealtimeWebSocket,
                "frame",
                "frame exceeds 64 MiB",
            ));
        }
        let send = async {
            self.writer
                .lock()
                .await
                .send(Message::Text(payload.into()))
                .await
                .map_err(|error| map_websocket_error(error, AuxiliaryPhase::Write))
        };
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(AuxiliaryError::Cancelled {
                operation: AuxiliaryOperation::RealtimeWebSocket,
                phase: AuxiliaryPhase::Write,
            }),
            result = tokio::time::timeout(self.write_timeout, send) => {
                result.map_err(|_| AuxiliaryError::Timeout {
                    operation: AuxiliaryOperation::RealtimeWebSocket,
                    phase: AuxiliaryPhase::Write,
                })?
            }
        }
    }
}

#[async_trait]
impl RealtimeSession for OpenAiRealtimeSession {
    async fn append_context(&self, text: &str) -> Result<(), AuxiliaryError> {
        self.append_context_with_channel(text, None).await
    }

    async fn append_context_with_channel(
        &self,
        text: &str,
        channel: Option<RealtimeContextAppendChannel>,
    ) -> Result<(), AuxiliaryError> {
        for message in self.codec.context_messages(text, channel)? {
            self.send_json(message).await?;
        }
        Ok(())
    }

    async fn send_audio(&self, frame: &RealtimeAudioFrame) -> Result<(), AuxiliaryError> {
        self.send_json(self.codec.audio_message(frame)?).await
    }

    async fn next_event(&self) -> Result<Option<RealtimeEvent>, AuxiliaryError> {
        if self.cancellation.is_cancelled() {
            return Err(cancelled_read_error());
        }
        if let Some(event) = self.pending.lock().await.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let next = async { self.reader.lock().await.next().await };
            let message = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(cancelled_read_error()),
                result = tokio::time::timeout(self.read_timeout, next) => {
                    result.map_err(|_| AuxiliaryError::Timeout {
                        operation: AuxiliaryOperation::RealtimeWebSocket,
                        phase: AuxiliaryPhase::Read,
                    })?
                }
            };
            match message {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(text))) => return self.codec.parse(&text).map(Some),
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    return Err(AuxiliaryError::Decode {
                        operation: AuxiliaryOperation::RealtimeWebSocket,
                        phase: AuxiliaryPhase::Read,
                        reason: "unexpected binary frame".into(),
                    });
                }
                Some(Err(error)) => return Err(map_websocket_error(error, AuxiliaryPhase::Read)),
            }
        }
    }

    async fn close(&self) -> Result<(), AuxiliaryError> {
        if self.codec.wire() == RealtimeWire::FramelessBidi && !self.cancellation.is_cancelled() {
            self.send_json(serde_json::json!({"type": "session.close"}))
                .await?;
        }
        let close = async {
            self.writer
                .lock()
                .await
                .send(Message::Close(None))
                .await
                .map_err(|error| map_websocket_error(error, AuxiliaryPhase::Close))?;
            loop {
                match self.reader.lock().await.next().await {
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed)) => {
                        return Ok(());
                    }
                    Some(Err(error)) => {
                        return Err(map_websocket_error(error, AuxiliaryPhase::Close));
                    }
                }
            }
        };
        tokio::time::timeout(self.close_timeout.min(Duration::from_secs(5)), close)
            .await
            .map_err(|_| AuxiliaryError::Timeout {
                operation: AuxiliaryOperation::RealtimeWebSocket,
                phase: AuxiliaryPhase::Close,
            })?
    }
}

pub(super) fn realtime_websocket_config(max_write_buffer: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .write_buffer_size(64 * 1024)
        .max_write_buffer_size(max_write_buffer)
        .max_message_size(Some(MAX_REALTIME_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_REALTIME_FRAME_BYTES))
}

pub(super) fn map_websocket_error(error: WebSocketError, phase: AuxiliaryPhase) -> AuxiliaryError {
    match error {
        WebSocketError::Http(response) => super::super::http::http_error(
            AuxiliaryOperation::RealtimeWebSocket,
            phase,
            response.status().as_u16(),
            response.headers(),
        ),
        WebSocketError::Capacity(_) => AuxiliaryError::Decode {
            operation: AuxiliaryOperation::RealtimeWebSocket,
            phase,
            reason: "WebSocket capacity limit exceeded".into(),
        },
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
            AuxiliaryError::Transport {
                operation: AuxiliaryOperation::RealtimeWebSocket,
                phase,
                kind: "closed",
            }
        }
        _ => AuxiliaryError::Transport {
            operation: AuxiliaryOperation::RealtimeWebSocket,
            phase,
            kind: "websocket",
        },
    }
}

fn cancelled_read_error() -> AuxiliaryError {
    AuxiliaryError::Cancelled {
        operation: AuxiliaryOperation::RealtimeWebSocket,
        phase: AuxiliaryPhase::Read,
    }
}
