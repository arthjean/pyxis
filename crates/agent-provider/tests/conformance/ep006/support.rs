use std::collections::BTreeMap;
use std::time::Duration;

use agent_auth::provider::ProviderCredential;
use agent_auth::{ProviderId, Secret};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryProvider};
use agent_core::model::{
    InputModality, ModelDescriptor, ModelToolMode, MultiAgentVersion, ReasoningReplaySupport,
    ResponsesDialect, TruncationMode, TruncationPolicy,
};
use agent_core::provider::{AuxiliaryCapabilities, Provider, ResponseItem};
use agent_provider::{
    ConfiguredOpenAiConfig, ConfiguredOpenAiProvider, OpenAiCatalogPolicy, OpenAiEndpointKind,
    ResponsesTransportConfig,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{AuxiliaryErrorExpectation, AuxiliaryTimeoutExpectation};

#[derive(Debug)]
pub(super) struct HttpCapture {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub(super) struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub(super) fn make_provider(
    base: &str,
    capabilities: AuxiliaryCapabilities,
) -> ConfiguredOpenAiProvider {
    make_provider_with_timeout(base, capabilities, Duration::from_secs(2))
}

pub(super) fn make_provider_with_timeout(
    base: &str,
    capabilities: AuxiliaryCapabilities,
    timeout: Duration,
) -> ConfiguredOpenAiProvider {
    let transport = ResponsesTransportConfig::new(base, "responses")
        .unwrap()
        .with_default_header("x-tenant", "fixture-tenant")
        .unwrap()
        .with_timeouts(timeout, timeout, timeout)
        .unwrap()
        .with_websocket_timeouts(timeout, timeout, timeout.min(Duration::from_secs(5)))
        .unwrap();
    let config = ConfiguredOpenAiConfig::new(
        "fixture",
        OpenAiEndpointKind::Standard,
        transport,
        OpenAiCatalogPolicy::Static(vec![descriptor()]),
    )
    .unwrap()
    .with_auxiliary_capabilities(capabilities);
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

pub(super) fn auxiliary(provider: &ConfiguredOpenAiProvider) -> &dyn AuxiliaryProvider {
    let provider: &dyn Provider = provider;
    provider
        .auxiliary()
        .expect("configured OpenAI exposes its auxiliary surface")
}

pub(super) fn response_item(value: Value) -> ResponseItem {
    ResponseItem::from_wire(&value).expect("valid response item fixture")
}

pub(super) async fn one_response_server(
    response: HttpResponse,
) -> (String, tokio::task::JoinHandle<HttpCapture>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let capture = read_http_request(&mut socket).await;
        write_http_response(&mut socket, &response).await;
        capture
    });
    (format!("http://{address}/v1/"), server)
}

pub(super) async fn stalling_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    (format!("http://{address}/v1/"), server)
}

pub(super) async fn read_http_request(socket: &mut TcpStream) -> HttpCapture {
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
    let mut request_line = lines.next().unwrap().split_whitespace();
    let method = request_line.next().unwrap().to_string();
    let target = request_line.next().unwrap().to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    HttpCapture {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    }
}

pub(super) async fn write_http_response(socket: &mut TcpStream, response: &HttpResponse) {
    let mut head = format!(
        "HTTP/1.1 {} OK\r\ncontent-length: {}\r\nconnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(&response.body).await.unwrap();
}

pub(super) fn assert_http_error(error: AuxiliaryError, expected: &AuxiliaryErrorExpectation) {
    assert!(matches!(
        error,
        AuxiliaryError::Http {
            operation,
            status,
            ..
        } if operation.to_string() == expected.operation && status == expected.status
    ));
}

pub(super) fn assert_unsupported(error: AuxiliaryError, operation: &str) {
    assert!(matches!(
        error,
        AuxiliaryError::Unsupported { operation: actual } if actual.to_string() == operation
    ));
}

pub(super) fn assert_timeout(error: AuxiliaryError, expected: &AuxiliaryTimeoutExpectation) {
    assert!(matches!(
        error,
        AuxiliaryError::Timeout {
            operation,
            phase,
        } if operation.to_string() == expected.operation && phase.to_string() == expected.phase
    ));
}

fn descriptor() -> ModelDescriptor {
    ModelDescriptor {
        slug: "test-model".into(),
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
