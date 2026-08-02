use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_auth::provider::ProviderCredential;
use agent_auth::{ProviderId, Secret};
use agent_core::auxiliary::AuxiliaryError;
use agent_core::auxiliary::realtime::{
    RealtimeAudioFrame, RealtimeCallRequest, RealtimeCallSessionConfig, RealtimeEvent,
    RealtimeFramelessSessionConfig, RealtimeOutputModality, RealtimeSessionConfig,
    RealtimeV1SessionConfig, RealtimeV2ConversationalSessionConfig, RealtimeV2SessionConfig,
    RealtimeVoice,
};
use agent_core::provider::AuxiliaryCapabilities;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request as WebSocketRequest, Response as WebSocketResponse,
};

use super::super::AuxiliaryCase;
use super::support::{
    HttpResponse, assert_http_error, assert_timeout, assert_unsupported, auxiliary, make_provider,
    make_provider_with_timeout, one_response_server, stalling_server,
};

pub(super) async fn assert_fixture(name: &str, case: &AuxiliaryCase) {
    let disabled = make_provider("http://127.0.0.1:9/v1/", AuxiliaryCapabilities::default());
    assert_unsupported(
        auxiliary(&disabled)
            .create_realtime_call(&call_request())
            .await
            .unwrap_err(),
        "realtime_call",
    );
    let connect_error = match auxiliary(&disabled).connect_realtime(v1_config()).await {
        Ok(_) => panic!("disabled Realtime capability unexpectedly connected"),
        Err(error) => error,
    };
    assert_unsupported(connect_error, "realtime_websocket");

    let (base, server) = one_response_server(HttpResponse {
        status: 200,
        headers: vec![("location".into(), "/v1/realtime/calls/rtc_fixture".into())],
        body: b"v=0\r\n".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    let call = auxiliary(&provider)
        .create_realtime_call(&call_request())
        .await
        .unwrap();
    let call_capture = server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upgrade = Arc::new(Mutex::new(UpgradeCapture::default()));
    let server_upgrade = Arc::clone(&upgrade);
    let websocket_server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(tcp, CaptureUpgrade(server_upgrade))
            .await
            .unwrap();
        let session: Value =
            serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"type": "response.output_text.delta", "delta": "hello"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let context: Value =
            serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        let close = socket.next().await.unwrap().unwrap();
        assert!(matches!(close, WebSocketMessage::Close(_)));
        socket.flush().await.unwrap();
        (session, context)
    });
    let websocket_provider = make_provider(&format!("http://{address}/v1/"), capabilities());
    let connection = auxiliary(&websocket_provider)
        .connect_realtime(v2_config())
        .await
        .unwrap();
    assert!(matches!(
        connection.next_event().await.unwrap(),
        Some(RealtimeEvent::OutputTranscriptDelta(text)) if text == "hello"
    ));
    connection.append_context("context").await.unwrap();
    connection.close().await.unwrap();
    let (session, context) = websocket_server.await.unwrap();
    let actual = {
        let upgrade = upgrade.lock().unwrap();
        json!({
            "call_path": call_capture.target,
            "call_method": call_capture.method,
            "multipart": call_capture.headers["content-type"].starts_with("multipart/form-data"),
            "has_session": String::from_utf8_lossy(&call_capture.body).contains("session"),
            "call_id": call.call_id,
            "sdp": call.sdp,
            "websocket_path": upgrade.path,
            "websocket_auth": upgrade.auth,
            "session_id": upgrade.session_id,
            "session_type": session["session"]["type"],
            "context_type": context["type"],
        })
    };
    assert_eq!(actual, case.success, "{name}: realtime success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider)
            .create_realtime_call(&call_request())
            .await
            .unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let result = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).connect_realtime(v1_config()),
    )
    .await
    .expect("realtime timeout fixture exceeded its bound");
    let error = match result {
        Ok(_) => panic!("realtime timeout fixture unexpectedly connected"),
        Err(error) => error,
    };
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        realtime: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn call_request() -> RealtimeCallRequest {
    RealtimeCallRequest {
        sdp: "v=0\r\n".into(),
        session: RealtimeCallSessionConfig::V1(v1_session()),
    }
}

fn v1_config() -> RealtimeSessionConfig {
    RealtimeSessionConfig::V1(v1_session())
}

fn v1_session() -> RealtimeV1SessionConfig {
    RealtimeV1SessionConfig {
        instructions: "Answer briefly.".into(),
        model: Some("gpt-realtime".into()),
        session_id: Some("session-1".into()),
        voice: RealtimeVoice::Cove,
    }
}

fn v2_config() -> RealtimeSessionConfig {
    RealtimeSessionConfig::RealtimeV2(RealtimeV2SessionConfig::Conversational(
        RealtimeV2ConversationalSessionConfig {
            instructions: "Answer briefly.".into(),
            model: Some("gpt-realtime".into()),
            session_id: Some("session-1".into()),
            output_modality: RealtimeOutputModality::Audio,
            voice: RealtimeVoice::Cove,
        },
    ))
}

#[derive(Default)]
struct UpgradeCapture {
    path: String,
    auth: bool,
    session_id: Option<String>,
}

struct CaptureUpgrade(Arc<Mutex<UpgradeCapture>>);

impl Callback for CaptureUpgrade {
    fn on_request(
        self,
        request: &WebSocketRequest,
        response: WebSocketResponse,
    ) -> Result<WebSocketResponse, ErrorResponse> {
        let mut capture = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        capture.path = request.uri().to_string();
        capture.auth = request.headers().contains_key("authorization");
        capture.session_id = request
            .headers()
            .get("x-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok(response)
    }
}

#[tokio::test]
async fn frameless_sideband_joins_without_reinitializing_and_cancels_on_scope_change() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let first = socket.next().await.unwrap().unwrap();
        assert!(matches!(first, WebSocketMessage::Close(_)));
        socket.flush().await.unwrap();
    });
    let provider = make_provider(&format!("http://{address}/v1/"), capabilities());
    let connection = auxiliary(&provider)
        .connect_realtime_sideband(
            RealtimeSessionConfig::FramelessBidi(RealtimeFramelessSessionConfig {
                instructions: "Answer briefly.".into(),
                initial_items: Vec::new(),
                delegation_ack_filler: None,
                model: Some("gpt-realtime".into()),
                session_id: Some("session-1".into()),
                voice: RealtimeVoice::Cove,
            }),
            "rtc_join",
        )
        .await
        .unwrap();
    provider
        .replace_credential(ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("rotated-secret"),
            identity: Some("fixture-account".into()),
        })
        .await
        .unwrap();
    assert!(matches!(
        connection.next_event().await,
        Err(AuxiliaryError::Cancelled { .. })
    ));
    connection.close().await.unwrap();
    server.await.unwrap();
}

#[test]
fn audio_debug_omits_payload() {
    let audio = RealtimeAudioFrame {
        data: "audio-secret".into(),
        sample_rate: 24_000,
        num_channels: 1,
        samples_per_channel: None,
        item_id: None,
    };
    assert!(!format!("{audio:?}").contains("audio-secret"));
}
