use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::chatgpt_http::ResponsesTransportConfig;

fn body(input: Vec<Value>) -> Value {
    serde_json::json!({
        "model": "gpt-5.5",
        "instructions": "code",
        "input": input,
        "tools": [],
        "stream": true,
        "store": false,
        "client_metadata": { "turn_id": "turn-1" }
    })
}

fn completed(id: &str) -> Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "status": "completed",
            "end_turn": true,
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }
    })
}

fn prepared(address: std::net::SocketAddr) -> PreparedWebSocketRequest {
    PreparedWebSocketRequest {
        endpoint: url::Url::parse(&format!("ws://{address}/responses")).expect("valid local URL"),
        headers: reqwest::header::HeaderMap::new(),
    }
}

fn policy() -> WebSocketPolicy {
    let config = ResponsesTransportConfig::new("http://127.0.0.1", "responses")
        .expect("valid local config")
        .with_timeouts(
            Duration::from_millis(500),
            Duration::from_millis(500),
            Duration::from_millis(500),
        )
        .expect("valid HTTP timeouts")
        .with_websocket_timeouts(
            Duration::from_millis(500),
            Duration::from_millis(500),
            Duration::from_millis(200),
        )
        .expect("valid WebSocket timeouts");
    WebSocketPolicy::from_config(&config)
}

fn spawn_actor() -> mpsc::Sender<SessionCommand> {
    let (commands, receiver) = mpsc::channel(1);
    tokio::spawn(run(receiver));
    commands
}

async fn start_stream(
    commands: &mpsc::Sender<SessionCommand>,
    address: std::net::SocketAddr,
    full_body: Value,
    replay_reasoning: bool,
    cancelled: CancellationToken,
) -> (
    Result<SessionOutcome, ProviderError>,
    mpsc::Receiver<Result<StreamEvent, ProviderError>>,
) {
    let (events, receiver) = mpsc::channel(32);
    let (ready, response) = oneshot::channel();
    commands
        .send(SessionCommand::Stream(StreamRequest {
            generation: 1,
            turn: TurnScope::Scoped("turn-1".into()),
            prepared: prepared(address),
            policy: policy(),
            full_body,
            replay_reasoning,
            provider_attempts: 2,
            cancelled,
            events,
            ready,
        }))
        .await
        .expect("actor accepts stream command");
    (
        response.await.expect("actor returns stream readiness"),
        receiver,
    )
}

async fn disconnect(commands: &mpsc::Sender<SessionCommand>) {
    let (completed, response) = oneshot::channel();
    commands
        .send(SessionCommand::Disconnect {
            policy: policy(),
            completed,
        })
        .await
        .expect("actor accepts disconnect");
    response.await.expect("actor completes disconnect");
}

#[tokio::test]
async fn actor_uses_the_configured_mapper_and_holds_one_response_in_flight() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type":"response.output_item.done",
                    "item": {
                        "type":"reasoning", "id":"rs_1", "status":"completed",
                        "encrypted_content":"opaque"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send reasoning");
        socket
            .send(Message::Text(completed("resp_1").to_string().into()))
            .await
            .expect("send terminal");
        while let Some(message) = socket.next().await {
            if matches!(message, Ok(Message::Close(_))) {
                break;
            }
        }
    });
    let commands = spawn_actor();
    let (ready, mut events) = start_stream(
        &commands,
        address,
        body(vec![
            serde_json::json!({"type":"message","role":"user","content":[]}),
        ]),
        true,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(ready.expect("stream opens"), SessionOutcome::Streaming);
    let mut collected = Vec::new();
    while let Some(event) = events.recv().await {
        collected.push(event.expect("valid mapped event"));
    }
    assert!(
        collected.iter().any(|event| matches!(
            event,
            StreamEvent::EncryptedReasoning { id, encrypted_content }
                if id == "rs_1" && encrypted_content == "opaque"
        )),
        "mapped events: {collected:?}"
    );
    assert_eq!(
        collected
            .iter()
            .filter(|event| matches!(event, StreamEvent::Done { .. }))
            .count(),
        1
    );
    disconnect(&commands).await;
    server.await.expect("server task");
}

#[tokio::test]
async fn missing_incremental_baseline_retries_once_with_the_full_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let user = serde_json::json!({"type":"message","role":"user","content":[]});
    let assistant = serde_json::json!({
        "type":"message", "id":"msg_1", "status":"completed",
        "role":"assistant", "content":[]
    });
    let normalized_assistant = serde_json::json!({
        "type":"message", "role":"assistant", "content":[]
    });
    let tool_result =
        serde_json::json!({"type":"function_call_output","call_id":"c","output":"ok"});
    let server_user = user.clone();
    let server_assistant = assistant.clone();
    let server_normalized_assistant = normalized_assistant.clone();
    let server_tool_result = tool_result.clone();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");

        let first = socket
            .next()
            .await
            .expect("first request")
            .expect("valid first request")
            .into_text()
            .expect("first request is text");
        let first: Value = serde_json::from_str(&first).expect("first request JSON");
        assert!(first.get("previous_response_id").is_none());
        assert_eq!(first["input"], serde_json::json!([server_user]));
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type":"response.output_item.done",
                    "item": server_assistant
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send assistant item");
        socket
            .send(Message::Text(completed("resp_old").to_string().into()))
            .await
            .expect("send first terminal");

        let incremental = socket
            .next()
            .await
            .expect("incremental request")
            .expect("valid incremental request")
            .into_text()
            .expect("incremental request is text");
        let incremental: Value =
            serde_json::from_str(&incremental).expect("incremental request JSON");
        assert_eq!(incremental["previous_response_id"], "resp_old");
        assert_eq!(
            incremental["input"],
            serde_json::json!([server_tool_result.clone()])
        );
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type":"error",
                    "error": {
                        "code":"previous_response_not_found",
                        "message":"baseline expired"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("reject incremental baseline");

        let retry = socket
            .next()
            .await
            .expect("full retry")
            .expect("valid full retry")
            .into_text()
            .expect("full retry is text");
        let retry: Value = serde_json::from_str(&retry).expect("full retry JSON");
        assert!(retry.get("previous_response_id").is_none());
        assert_eq!(
            retry["input"],
            serde_json::json!([server_user, server_normalized_assistant, server_tool_result])
        );
        socket
            .send(Message::Text(completed("resp_new").to_string().into()))
            .await
            .expect("send retry terminal");
        while let Some(message) = socket.next().await {
            if matches!(message, Ok(Message::Close(_))) {
                break;
            }
        }
    });
    let commands = spawn_actor();
    let (first_ready, mut first_events) = start_stream(
        &commands,
        address,
        body(vec![user.clone()]),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        first_ready.expect("first stream opens"),
        SessionOutcome::Streaming
    );
    while first_events.recv().await.is_some() {}

    let (second_ready, mut second_events) = start_stream(
        &commands,
        address,
        body(vec![user, normalized_assistant, tool_result]),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        second_ready.expect("second stream opens"),
        SessionOutcome::Streaming
    );
    let mut done = 0;
    while let Some(event) = second_events.recv().await {
        if matches!(event.expect("valid retry event"), StreamEvent::Done { .. }) {
            done += 1;
        }
    }
    assert_eq!(done, 1);
    disconnect(&commands).await;
    server.await.expect("server task");
}

#[tokio::test]
async fn a_late_missing_baseline_never_replays_published_events() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket
            .send(Message::Text(completed("resp_old").to_string().into()))
            .await
            .expect("send continuation baseline");
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"partial"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send accepted delta");
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type":"error",
                    "error": {
                        "code":"previous_response_not_found",
                        "message":"baseline expired"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("reject baseline after a delta");
        while let Some(message) = socket.next().await {
            if matches!(&message, Ok(Message::Close(_))) {
                break;
            }
            assert!(
                !matches!(message, Ok(Message::Text(_))),
                "published events must never be replayed"
            );
        }
    });
    let commands = spawn_actor();
    let (first_ready, mut first_events) = start_stream(
        &commands,
        address,
        body(Vec::new()),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        first_ready.expect("first stream opens"),
        SessionOutcome::Streaming
    );
    while first_events.recv().await.is_some() {}

    let (second_ready, mut receiver) = start_stream(
        &commands,
        address,
        body(vec![serde_json::json!({
            "type":"function_call_output","call_id":"c","output":"ok"
        })]),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        second_ready.expect("second stream opens"),
        SessionOutcome::Streaming
    );
    assert!(matches!(
        receiver.recv().await,
        Some(Ok(StreamEvent::TextDelta { ref text })) if text == "partial"
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(Err(ProviderError::Api {
            category: ProviderErrorCategory::Failed,
            status: None,
            ..
        }))
    ));
    server.await.expect("server task");
}

#[tokio::test]
async fn preconnect_426_enables_sticky_http_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.expect("accept connection");
        let mut request = [0_u8; 8192];
        let read = tcp.read(&mut request).await.expect("read upgrade request");
        assert!(read > 0);
        tcp.write_all(
            b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write rejected upgrade");
    });
    let commands = spawn_actor();
    let (ready, response) = oneshot::channel();
    commands
        .send(SessionCommand::Preconnect(PreconnectRequest {
            generation: 1,
            turn: TurnScope::Scoped("turn-1".into()),
            prepared: prepared(address),
            policy: policy(),
            cancelled: CancellationToken::new(),
            ready,
        }))
        .await
        .expect("actor accepts preconnect");
    response
        .await
        .expect("preconnect response")
        .expect("426 selects fallback without failing preconnect");

    let (ready, _events) = start_stream(
        &commands,
        address,
        body(Vec::new()),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(ready.expect("stream outcome"), SessionOutcome::FallbackHttp);
    server.await.expect("server task");
}

#[tokio::test]
async fn cancellation_before_response_create_closes_without_dispatching() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");
        let first = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("client closes promptly");
        assert!(matches!(first, Some(Ok(Message::Close(_)))));
    });
    let commands = spawn_actor();
    let (ready, response) = oneshot::channel();
    commands
        .send(SessionCommand::Preconnect(PreconnectRequest {
            generation: 1,
            turn: TurnScope::Scoped("turn-1".into()),
            prepared: prepared(address),
            policy: policy(),
            cancelled: CancellationToken::new(),
            ready,
        }))
        .await
        .expect("actor accepts preconnect");
    response
        .await
        .expect("preconnect response")
        .expect("preconnect succeeds");

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let (ready, _events) =
        start_stream(&commands, address, body(Vec::new()), false, cancelled).await;
    assert!(matches!(ready, Err(ProviderError::Stream(_))));
    server.await.expect("server task");
}

#[tokio::test]
async fn dropping_the_session_actor_closes_an_idle_connection() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");
        let first = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("actor closes promptly");
        assert!(matches!(first, Some(Ok(Message::Close(_)))));
    });
    let commands = spawn_actor();
    let (ready, response) = oneshot::channel();
    commands
        .send(SessionCommand::Preconnect(PreconnectRequest {
            generation: 1,
            turn: TurnScope::Scoped("turn-1".into()),
            prepared: prepared(address),
            policy: policy(),
            cancelled: CancellationToken::new(),
            ready,
        }))
        .await
        .expect("actor accepts preconnect");
    response
        .await
        .expect("preconnect response")
        .expect("preconnect succeeds");

    drop(commands);
    server.await.expect("server task");
}

#[tokio::test]
async fn post_dispatch_disconnect_is_terminal_and_never_falls_back_inline() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept connection");
        let mut socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("accept websocket");
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        socket.close(None).await.expect("close after dispatch");
    });
    let commands = spawn_actor();
    let (ready, mut events) = start_stream(
        &commands,
        address,
        body(Vec::new()),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(ready.expect("stream opens"), SessionOutcome::Streaming);
    let error = events
        .recv()
        .await
        .expect("terminal error event")
        .expect_err("disconnect is not a success");
    assert!(matches!(
        error,
        ProviderError::Api {
            category: ProviderErrorCategory::Failed,
            status: None,
            ..
        }
    ));
    server.await.expect("server task");
}

#[tokio::test]
async fn semantic_failure_does_not_consume_the_transport_failure_budget() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (first, _) = listener.accept().await.expect("accept first connection");
        let mut first = tokio_tungstenite::accept_async(first)
            .await
            .expect("accept first websocket");
        assert!(matches!(first.next().await, Some(Ok(Message::Text(_)))));
        first
            .send(Message::Text(
                serde_json::json!({
                    "type":"error",
                    "error":{"code":"invalid_prompt","message":"bad prompt"}
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send semantic error");
        while let Some(message) = first.next().await {
            if matches!(message, Ok(Message::Close(_))) {
                break;
            }
        }

        let (second, _) = listener.accept().await.expect("accept second connection");
        let mut second = tokio_tungstenite::accept_async(second)
            .await
            .expect("accept second websocket");
        assert!(matches!(second.next().await, Some(Ok(Message::Text(_)))));
        second
            .send(Message::Text(completed("resp_2").to_string().into()))
            .await
            .expect("send second terminal");
        while let Some(message) = second.next().await {
            if matches!(message, Ok(Message::Close(_))) {
                break;
            }
        }
    });
    let commands = spawn_actor();
    let (first_ready, mut first_events) = start_stream(
        &commands,
        address,
        body(Vec::new()),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        first_ready.expect("first stream opens"),
        SessionOutcome::Streaming
    );
    assert!(matches!(
        first_events.recv().await,
        Some(Err(ProviderError::Api {
            category: ProviderErrorCategory::InvalidPrompt,
            ..
        }))
    ));

    let (second_ready, mut second_events) = start_stream(
        &commands,
        address,
        body(Vec::new()),
        false,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        second_ready.expect("second stream opens"),
        SessionOutcome::Streaming
    );
    assert!(matches!(
        second_events.recv().await,
        Some(Ok(StreamEvent::ResponseMetadata { .. })) | Some(Ok(StreamEvent::Done { .. }))
    ));
    while second_events.recv().await.is_some() {}
    disconnect(&commands).await;
    server.await.expect("server task");
}

#[test]
fn unscoped_requests_never_reuse_continuation_state() {
    let previous_body = body(Vec::new());
    let mut capture = ResponseCapture::default();
    capture_response_state(&completed("resp_old"), &mut capture, &mut None)
        .expect("capture response");
    let mut state = SessionState {
        turn_id: None,
        turn_state: Some("sticky".into()),
        continuation: capture.into_continuation(&previous_body),
        ..SessionState::default()
    };

    synchronize_turn(&mut state, TurnScope::Unscoped);

    assert!(state.turn_id.is_none());
    assert!(state.turn_state.is_none());
    assert!(state.continuation.is_none());
}

#[test]
fn transport_failures_reserve_the_last_attempt_for_http() {
    let mut state = SessionState {
        failure_budget: 3,
        ..SessionState::default()
    };
    assert!(!record_transport_failure(&mut state));
    assert!(!record_transport_failure(&mut state));
    assert!(record_transport_failure(&mut state));
    assert!(state.fallback_http);
}
