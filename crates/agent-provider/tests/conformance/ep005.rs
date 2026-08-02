use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_auth::provider::ProviderCredential;
use agent_auth::{ProviderId, Secret};
use agent_core::model::{
    InputModality, ModelDescriptor, ModelToolMode, MultiAgentVersion, ReasoningReplaySupport,
    ResponsesDialect, TruncationMode, TruncationPolicy,
};
use agent_core::provider::{Capabilities, CapabilityLimits, Provider, ToolCallingCapabilities};
use agent_provider::{
    AmazonBedrockConfig, AmazonBedrockProvider, ConfiguredOpenAiConfig, ConfiguredOpenAiProvider,
    OpenAiCatalogPolicy, OpenAiEndpointKind, ResponsesTransportConfig,
};
use aws_credential_types::Credentials;
use aws_types::region::Region;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};

use super::ProviderCase;

struct HttpCapture {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Default)]
struct WebSocketUpgrade {
    path: String,
    configured_header: Option<String>,
    beta_header: Option<String>,
    auth_header_present: bool,
}

struct CaptureUpgrade(Arc<Mutex<WebSocketUpgrade>>);

impl Callback for CaptureUpgrade {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let mut capture = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        capture.path = request.uri().to_string();
        capture.configured_header = request
            .headers()
            .get("x-tenant")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        capture.beta_header = request
            .headers()
            .get("openai-beta")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        capture.auth_header_present = request.headers().contains_key("authorization");
        Ok(response)
    }
}

pub(super) async fn assert_wire_snapshot(name: &str, case: &ProviderCase) {
    let actual = match case.wire.as_str() {
        "configured_responses" => configured_snapshot(case).await,
        "bedrock_converse_stream" => bedrock_snapshot(case).await,
        wire => panic!("{name}: unsupported provider wire `{wire}`"),
    };
    assert_eq!(actual, case.expected, "{name}: provider wire snapshot");
}

async fn configured_snapshot(case: &ProviderCase) -> Value {
    let http = configured_http_snapshot(case).await;
    let websocket = configured_websocket_snapshot(case).await;
    json!({"http_sse": http, "websocket": websocket})
}

async fn configured_http_snapshot(case: &ProviderCase) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let capture = read_http_request(&mut socket).await;
        let event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_ep005",
                "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        });
        let body = format!("data: {event}\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        capture
    });
    let provider = configured_provider(&format!("http://{address}/v1/"), false);
    let events: Vec<_> = provider
        .stream(case.canonical.clone())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    snapshot_http(server.await.unwrap())
}

async fn configured_websocket_snapshot(case: &ProviderCase) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upgrade = Arc::new(Mutex::new(WebSocketUpgrade::default()));
    let server_upgrade = Arc::clone(&upgrade);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(tcp, CaptureUpgrade(server_upgrade))
            .await
            .unwrap();
        let body: Value =
            serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_ep005",
                        "status": "completed",
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        body
    });
    let provider = configured_provider(&format!("http://{address}/v1/"), true);
    let events: Vec<_> = provider
        .stream(case.canonical.clone())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let body = server.await.unwrap();
    provider.disconnect_auth().await.unwrap();
    let upgrade = upgrade.lock().unwrap();
    json!({
        "path": upgrade.path,
        "configured_headers": {"x-tenant": upgrade.configured_header},
        "auth_header_present": upgrade.auth_header_present,
        "beta_header": upgrade.beta_header,
        "body": body,
    })
}

async fn bedrock_snapshot(case: &ProviderCase) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let capture = read_http_request(&mut socket).await;
        let body = r#"{"message":"fixture rejection"}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nx-amzn-errortype: ValidationException\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        capture
    });
    let sdk = aws_sdk_bedrockruntime::Config::builder()
        .region(Region::new("eu-west-3"))
        .credentials_provider(Credentials::new(
            "fixture-access",
            "fixture-secret",
            None,
            None,
            "ep005-fixture",
        ))
        .endpoint_url(format!("http://{address}"))
        .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
        .build();
    let provider = AmazonBedrockProvider::from_client(
        bedrock_config(),
        aws_sdk_bedrockruntime::Client::from_conf(sdk),
    )
    .unwrap();
    assert!(provider.stream(case.canonical.clone()).await.is_err());
    snapshot_http(server.await.unwrap())
}

fn snapshot_http(capture: HttpCapture) -> Value {
    json!({
        "path": capture.path,
        "configured_headers": capture
            .headers
            .get("x-tenant")
            .map(|value| json!({"x-tenant": value})),
        "auth_header_present": capture.headers.contains_key("authorization"),
        "body": capture.body,
    })
}

async fn read_http_request(socket: &mut TcpStream) -> HttpCapture {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let content_length: usize = headers["content-length"].parse().unwrap();
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
    HttpCapture {
        path,
        headers,
        body,
    }
}

fn configured_provider(base: &str, websocket: bool) -> ConfiguredOpenAiProvider {
    let transport = ResponsesTransportConfig::new(base, "responses")
        .unwrap()
        .with_default_header("x-tenant", "fixture-tenant")
        .unwrap()
        .with_websocket(websocket);
    let config = ConfiguredOpenAiConfig::new(
        "fixture",
        OpenAiEndpointKind::Standard,
        transport,
        OpenAiCatalogPolicy::Static(vec![descriptor("test-model")]),
    )
    .unwrap();
    ConfiguredOpenAiProvider::new(
        config,
        ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new("wire-secret"),
            identity: Some("fixture-account".into()),
        },
        None,
    )
    .unwrap()
}

fn bedrock_config() -> AmazonBedrockConfig {
    let capabilities = Capabilities {
        tools: true,
        structured_output: true,
        max_context: 8_192,
        limits: CapabilityLimits {
            max_tool_schema_bytes: Some(64 * 1024),
            ..CapabilityLimits::default()
        },
        tool_calling: ToolCallingCapabilities {
            strict_json_schema: true,
            parallel_tool_calls: true,
            ..ToolCallingCapabilities::default()
        },
        ..Capabilities::default()
    };
    AmazonBedrockConfig::new("eu-west-3", vec![descriptor("anthropic.claude-test-v1:0")])
        .unwrap()
        .with_capabilities(capabilities)
        .unwrap()
}

fn descriptor(slug: &str) -> ModelDescriptor {
    ModelDescriptor {
        slug: slug.into(),
        display_name: "Fixture model".into(),
        instructions: "Be useful.".into(),
        context_window: 8_192,
        auto_compact_token_limit: 7_000,
        input_modalities: vec![InputModality::Text],
        supports_reasoning: false,
        default_reasoning_effort: None,
        supported_reasoning_efforts: Vec::new(),
        supports_verbosity: false,
        default_verbosity: None,
        supports_parallel_tool_calls: true,
        tool_capabilities: Default::default(),
        service_tiers: Vec::new(),
        reasoning_replay: ReasoningReplaySupport::Disabled,
        responses_dialect: ResponsesDialect::Standard,
        tool_mode: ModelToolMode::Direct,
        multi_agent_version: MultiAgentVersion::Disabled,
        truncation: TruncationPolicy {
            mode: TruncationMode::Tokens,
            limit: 1_000,
        },
        comp_hash: None,
    }
}
