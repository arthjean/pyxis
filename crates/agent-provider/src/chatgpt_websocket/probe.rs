//! Explicit, sanitized live proof of the private ChatGPT WebSocket contract.

use std::time::Duration;

use agent_core::provider::{ProviderError, StreamEvent};
use agent_core::redaction::is_sensitive_key;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_http::{PreparedWebSocketRequest, ResponsesTransportConfig};

use super::WebSocketProbeExecution;
use super::continuation::{
    ContinuationInput, GenerationMode, ResponseCapture, capture_response_state,
    response_create_body, validate_turn_state,
};
use super::transport::{
    Socket, WebSocketPolicy, close_socket, connect, http_status, map_websocket_error, send_text,
    validate_message_bytes,
};

const PROBE_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Explicit capability required by the live probe API. Constructing this value
/// is the operator opt-in; ordinary provider construction never runs a probe.
#[derive(Debug, Clone, Copy)]
pub struct WebSocketProbeAuthorization(());

impl WebSocketProbeAuthorization {
    pub fn explicitly_authorized() -> Self {
        Self(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSocketProbeVerdict {
    Capable,
    AuthenticationRejected,
    IncompatibleUpgrade,
    CapabilityAbsent,
    TimedOut,
    Failed,
}

/// Sanitized live evidence. It deliberately carries booleans and header names,
/// never credentials, account identifiers, response IDs, or response payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketProbeReport {
    pub verdict: WebSocketProbeVerdict,
    pub url: String,
    pub upgrade_status: Option<u16>,
    pub non_sensitive_request_headers: Vec<String>,
    pub metadata_present: bool,
    pub prewarm_terminal: bool,
    pub previous_response_reused: bool,
    pub turn_state_stable: Option<bool>,
    pub actual_terminal: bool,
    pub close_code: Option<u16>,
}

pub(super) async fn run(execution: WebSocketProbeExecution<'_>) -> WebSocketProbeReport {
    let WebSocketProbeExecution {
        authorization: _authorization,
        auth,
        config,
        request,
        route,
        full_body,
        body_bytes,
    } = execution;
    let mut report = empty_report(config);
    let policy = WebSocketPolicy::from_config(config);
    let result = tokio::time::timeout(PROBE_TOTAL_TIMEOUT, async {
        validate_message_bytes(body_bytes)?;
        let prepared = config.prepare_websocket(route, auth)?;
        report.non_sensitive_request_headers = non_sensitive_header_names(&prepared);
        let mut connection = connect(prepared, policy).await?;
        report.upgrade_status = Some(101);
        let initial_turn_state = connection
            .handshake_metadata()
            .and_then(|metadata| metadata.turn_state.as_deref())
            .map(validate_turn_state)
            .transpose()?;
        report.metadata_present = connection.handshake_metadata().is_some();

        let operation = async {
            let warmup = response_create_body(
                &full_body,
                ContinuationInput::Full,
                initial_turn_state.as_deref(),
                GenerationMode::Prewarm,
            );
            send_text(
                &mut connection.socket,
                serialize_probe_request(&warmup)?,
                policy.write_timeout,
            )
            .await?;
            let prewarm =
                read_terminal(&mut connection.socket, policy, request.reasoning_replay).await?;
            report.prewarm_terminal = true;

            let continuation_turn_state = prewarm
                .observed_turn_state
                .as_deref()
                .or(initial_turn_state.as_deref());
            let empty_input: &[Value] = &[];
            let continuation = prewarm.response_id.as_deref().map_or(
                ContinuationInput::Full,
                |previous_response_id| ContinuationInput::Incremental {
                    previous_response_id,
                    input: empty_input,
                },
            );
            let actual = response_create_body(
                &full_body,
                continuation,
                continuation_turn_state,
                GenerationMode::Generate,
            );
            send_text(
                &mut connection.socket,
                serialize_probe_request(&actual)?,
                policy.write_timeout,
            )
            .await?;
            let actual =
                read_terminal(&mut connection.socket, policy, request.reasoning_replay).await?;
            report.previous_response_reused = prewarm.response_id.is_some();
            report.actual_terminal = true;
            report.turn_state_stable = compare_turn_state(
                prewarm
                    .observed_turn_state
                    .as_deref()
                    .or(initial_turn_state.as_deref()),
                actual.observed_turn_state.as_deref(),
            );
            Ok::<(), ProviderError>(())
        }
        .await;
        report.close_code = close_socket(&mut connection.socket, policy.close_timeout).await;
        operation
    })
    .await;

    match result {
        Err(_) => report.verdict = WebSocketProbeVerdict::TimedOut,
        Ok(Ok(())) => report.verdict = WebSocketProbeVerdict::Capable,
        Ok(Err(error)) => {
            report.upgrade_status = http_status(&error).or(report.upgrade_status);
            report.verdict = probe_verdict(&error);
        }
    }
    report
}

struct ProbeTerminal {
    response_id: Option<String>,
    observed_turn_state: Option<String>,
}

async fn read_terminal(
    socket: &mut Socket,
    policy: WebSocketPolicy,
    replay_reasoning: bool,
) -> Result<ProbeTerminal, ProviderError> {
    let mut capture = ResponseCapture::default();
    let mut observed_turn_state = None;
    let mut mapper = CodexEventMapper::with_replay(replay_reasoning);
    loop {
        let message = tokio::time::timeout(policy.idle_timeout, socket.next())
            .await
            .map_err(|_| ProviderError::Stream("websocket probe idle timeout".into()))?
            .ok_or_else(|| ProviderError::Stream("websocket probe closed before terminal".into()))?
            .map_err(map_websocket_error)?;
        match message {
            Message::Text(text) => {
                let raw: Value = serde_json::from_str(&text)
                    .map_err(|_| ProviderError::Decode("malformed websocket probe event".into()))?;
                capture_response_state(&raw, &mut capture, &mut observed_turn_state)?;
                let terminal = mapper
                    .ingest(&text)?
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Done { .. }));
                if terminal {
                    let response_id = capture.response_id().map(str::to_string);
                    return Ok(ProbeTerminal {
                        response_id,
                        observed_turn_state,
                    });
                }
            }
            Message::Ping(payload) => {
                tokio::time::timeout(policy.write_timeout, socket.send(Message::Pong(payload)))
                    .await
                    .map_err(|_| ProviderError::Stream("websocket probe pong timeout".into()))?
                    .map_err(map_websocket_error)?
            }
            Message::Pong(_) => {}
            Message::Binary(_) => {
                return Err(ProviderError::Decode(
                    "unexpected binary websocket probe frame".into(),
                ));
            }
            Message::Close(_) | Message::Frame(_) => {
                return Err(ProviderError::Stream(
                    "websocket probe closed before terminal".into(),
                ));
            }
        }
    }
}

fn compare_turn_state(previous: Option<&str>, current: Option<&str>) -> Option<bool> {
    match (previous, current) {
        (Some(previous), Some(current)) => Some(previous == current),
        _ => None,
    }
}

fn serialize_probe_request(body: &Value) -> Result<String, ProviderError> {
    serde_json::to_string(body)
        .map_err(|_| ProviderError::Decode("websocket probe serialization failed".into()))
}

fn empty_report(config: &ResponsesTransportConfig) -> WebSocketProbeReport {
    WebSocketProbeReport {
        verdict: WebSocketProbeVerdict::Failed,
        url: sanitized_probe_url(config),
        upgrade_status: None,
        non_sensitive_request_headers: Vec::new(),
        metadata_present: false,
        prewarm_terminal: false,
        previous_response_reused: false,
        turn_state_stable: None,
        actual_terminal: false,
        close_code: None,
    }
}

fn probe_verdict(error: &ProviderError) -> WebSocketProbeVerdict {
    match http_status(error) {
        Some(401 | 403) => WebSocketProbeVerdict::AuthenticationRejected,
        Some(426) => WebSocketProbeVerdict::IncompatibleUpgrade,
        Some(404 | 405) => WebSocketProbeVerdict::CapabilityAbsent,
        _ => WebSocketProbeVerdict::Failed,
    }
}

fn sanitized_probe_url(config: &ResponsesTransportConfig) -> String {
    let Ok(mut endpoint) = config.endpoint() else {
        return "<invalid-websocket-url>".into();
    };
    let scheme = match endpoint.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => return "<invalid-websocket-url>".into(),
    };
    if endpoint.set_scheme(scheme).is_err() {
        return "<invalid-websocket-url>".into();
    }
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    endpoint.to_string()
}

fn non_sensitive_header_names(prepared: &PreparedWebSocketRequest) -> Vec<String> {
    let mut names: Vec<String> = prepared
        .headers
        .keys()
        .map(|name| name.as_str().to_ascii_lowercase())
        .filter(|name| !is_sensitive_key(name) && name != "proxy-authorization")
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_never_contains_secret_header_names() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer secret".parse().expect("valid header"),
        );
        headers.insert("chatgpt-account-id", "acct".parse().expect("valid header"));
        headers.insert("x-api-key", "secret".parse().expect("valid header"));
        headers.insert(
            "openai-beta",
            "responses_websockets=2026-02-06"
                .parse()
                .expect("valid header"),
        );
        let prepared = PreparedWebSocketRequest {
            endpoint: url::Url::parse("wss://example.test/responses?secret=x").expect("valid URL"),
            headers,
        };
        assert_eq!(
            non_sensitive_header_names(&prepared),
            vec!["openai-beta".to_string()]
        );
    }

    #[test]
    fn stability_requires_two_observations_and_detects_changes() {
        assert_eq!(compare_turn_state(Some("a"), Some("a")), Some(true));
        assert_eq!(compare_turn_state(Some("a"), Some("b")), Some(false));
        assert_eq!(compare_turn_state(Some("a"), None), None);
        assert_eq!(PROBE_TOTAL_TIMEOUT, Duration::from_secs(60));
    }

    #[test]
    fn verdict_distinguishes_auth_upgrade_and_absent_capability() {
        let error = |status| ProviderError::Http {
            status,
            message: "redacted".into(),
            retry_after_ms: None,
        };
        assert_eq!(
            probe_verdict(&error(401)),
            WebSocketProbeVerdict::AuthenticationRejected
        );
        assert_eq!(
            probe_verdict(&error(426)),
            WebSocketProbeVerdict::IncompatibleUpgrade
        );
        assert_eq!(
            probe_verdict(&error(404)),
            WebSocketProbeVerdict::CapabilityAbsent
        );
    }
}
