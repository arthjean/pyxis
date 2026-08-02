//! Provider-neutral execution engine for OpenAI Responses-compatible wires.

pub(crate) mod catalog;
mod error;
mod guard;

use agent_auth::provider::ProviderRequestAuth;
use agent_core::model::ResolvedModelRuntime;
use agent_core::provider::{CanonicalRequest, ProviderError, StreamEvent};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, stream::BoxStream};
use tokio_util::sync::CancellationToken;

use crate::chatgpt_error::{from_http_response, invalid_request};
use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_http::{PreparedResponsesRoute, ResponsesTransportConfig};
use crate::chatgpt_metadata::response_metadata_from_headers;
use crate::chatgpt_request::{ResponsesBodyOptions, build_responses_body, inject_cache_key};
use crate::chatgpt_websocket::{ResponsesWebSocket, WebSocketExecution, WebSocketOutcome};

pub(crate) use error::{classify_error, reasoning_effort_for_request};
use guard::cancellation_guarded;
pub(crate) use guard::{idle_guarded, send_with_header_timeout};

pub(crate) struct ResponsesExecution<'a> {
    pub(crate) http: &'a reqwest::Client,
    pub(crate) websocket: &'a ResponsesWebSocket,
    pub(crate) config: &'a ResponsesTransportConfig,
    pub(crate) auth: &'a ProviderRequestAuth,
}

pub(crate) struct ResponsesPlan {
    request: CanonicalRequest,
    runtime: ResolvedModelRuntime,
    body: serde_json::Value,
    body_bytes: Vec<u8>,
    route: PreparedResponsesRoute,
}

impl ResponsesPlan {
    pub(crate) fn request(&self) -> &CanonicalRequest {
        &self.request
    }

    pub(crate) fn body(&self) -> &serde_json::Value {
        &self.body
    }

    pub(crate) fn body_bytes(&self) -> &[u8] {
        &self.body_bytes
    }

    pub(crate) fn route(&self) -> &PreparedResponsesRoute {
        &self.route
    }
}

pub(crate) fn prepare<F>(
    config: &ResponsesTransportConfig,
    capabilities: &agent_core::provider::Capabilities,
    mut request: CanonicalRequest,
    resolve_runtime: F,
    store: bool,
    prompt_cache_key: Option<&str>,
) -> Result<ResponsesPlan, ProviderError>
where
    F: FnOnce(&CanonicalRequest) -> Result<ResolvedModelRuntime, ProviderError>,
{
    request.validate().map_err(invalid_request)?;
    let runtime = resolve_runtime(&request)?;
    runtime
        .ensure_tools_supported(&request.tools)
        .map_err(|error| ProviderError::UnsupportedTool {
            tool: error.tool,
            reason: error.reason,
        })?;
    if request.model_runtime.is_none() {
        request.reasoning_effort = runtime.reasoning_effort.clone();
        request.model_runtime = Some(runtime.clone());
    }
    request.validate().map_err(invalid_request)?;
    capabilities.ensure_request_supported(&request)?;
    let body = build_body(&request, &runtime, store, prompt_cache_key);
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|_| ProviderError::Decode("responses request serialization failed".into()))?;
    let route = config.prepare_route(&request, runtime.responses_dialect)?;
    Ok(ResponsesPlan {
        request,
        runtime,
        body,
        body_bytes,
        route,
    })
}

pub(crate) fn build_body(
    request: &CanonicalRequest,
    runtime: &ResolvedModelRuntime,
    store: bool,
    prompt_cache_key: Option<&str>,
) -> serde_json::Value {
    let reasoning_effort = runtime
        .reasoning_effort
        .as_deref()
        .map(reasoning_effort_for_request);
    let mut body = build_responses_body(
        request,
        ResponsesBodyOptions {
            reasoning_effort,
            include_encrypted_reasoning: request.reasoning_replay
                && runtime.reasoning_effort.is_some(),
            parallel_tool_calls: runtime.supports_parallel_tool_calls,
            text_verbosity: runtime
                .supports_verbosity
                .then_some(runtime.verbosity.as_deref())
                .flatten(),
            dialect: runtime.responses_dialect,
        },
    );
    body["store"] = serde_json::Value::Bool(store);
    if body.get("prompt_cache_key").is_none()
        && let Some(key) = prompt_cache_key
    {
        inject_cache_key(&mut body, key);
    }
    body
}

pub(crate) async fn stream(
    execution: ResponsesExecution<'_>,
    plan: ResponsesPlan,
    cancellation: CancellationToken,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let ResponsesExecution {
        http,
        websocket,
        config,
        auth,
    } = execution;
    let ResponsesPlan {
        request,
        runtime,
        body,
        body_bytes,
        route,
    } = plan;

    if config.websocket_enabled() {
        match websocket
            .stream(WebSocketExecution {
                auth,
                config,
                request: &request,
                route: &route,
                full_body: body,
                body_bytes: &body_bytes,
                provider_attempts: runtime.retry.max_attempts,
            })
            .await?
        {
            WebSocketOutcome::Stream(stream) => {
                return Ok(cancellation_guarded(stream, cancellation).boxed());
            }
            WebSocketOutcome::FallbackHttp => {
                tracing::warn!(
                    target: "pyxis::provider",
                    "Responses WebSocket unavailable for this session; using HTTP/SSE"
                );
            }
        }
    }

    let prepared_http = config.prepare_http(route, &body_bytes)?;
    let http_request = prepared_http.authorize(http, auth)?;
    let response = send_with_header_timeout(http, http_request, config.header_timeout()).await?;
    if !response.status().is_success() {
        return Err(from_http_response(response).await);
    }

    let quotas = crate::quota::parse_all_quota_headers(response.headers());
    let metadata = response_metadata_from_headers(response.headers());
    let mut events = response.bytes_stream().eventsource();
    let replay = request.reasoning_replay;
    let mapped = async_stream::stream! {
        for snapshot in quotas {
            yield Ok(StreamEvent::Quota { snapshot });
        }
        if !metadata.is_empty() {
            yield Ok(StreamEvent::ResponseMetadata { metadata: Box::new(metadata) });
        }
        let mut mapper = CodexEventMapper::with_replay(replay);
        loop {
            match events.next().await {
                Some(Ok(event)) => match mapper.ingest(&event.data) {
                    Ok(mapped_events) => {
                        for event in mapped_events {
                            let terminal = matches!(event, StreamEvent::Done { .. });
                            yield Ok(event);
                            if terminal {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                },
                Some(Err(_)) => {
                    yield Err(ProviderError::Stream("responses event stream failed".into()));
                    return;
                }
                None => {
                    yield Err(ProviderError::Stream("missing terminal event".into()));
                    return;
                }
            }
        }
    };
    Ok(cancellation_guarded(
        idle_guarded(mapped.boxed(), config.idle_timeout()).boxed(),
        cancellation,
    )
    .boxed())
}

#[cfg(test)]
mod tests {
    use agent_core::message::Message;
    use agent_core::model::ModelRetryPolicy;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

    use super::*;
    use crate::models::ModelCatalog;

    fn request_and_runtime() -> (CanonicalRequest, ResolvedModelRuntime) {
        let runtime = ModelCatalog::embedded()
            .resolve(
                "gpt-5.5",
                None,
                100,
                ModelRetryPolicy {
                    max_attempts: 2,
                    backoff_base_ms: 10,
                },
            )
            .expect("embedded runtime");
        let request = CanonicalRequest {
            model: runtime.slug.clone(),
            model_runtime: Some(runtime.clone()),
            reasoning_effort: runtime.reasoning_effort.clone(),
            messages: vec![Message::user("hello")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        };
        (request, runtime)
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or_default();
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "request ended before body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(bytes).expect("HTTP request is UTF-8")
    }

    #[tokio::test]
    async fn http_sse_uses_the_shared_mapper_and_preserves_response_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            let event = serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_http",
                    "status": "completed",
                    "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
                }
            });
            let body = format!("data: {event}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-models-etag: etag-http\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("respond");
            request
        });
        let config = ResponsesTransportConfig::new(&format!("http://{address}/"), "responses")
            .expect("config")
            .with_websocket(false);
        let (request, runtime) = request_and_runtime();
        let plan = prepare(
            &config,
            &agent_core::provider::Capabilities {
                reasoning: true,
                ..agent_core::provider::Capabilities::default()
            },
            request,
            |_| Ok(runtime),
            false,
            Some("cache-key"),
        )
        .expect("plan");
        let auth = ProviderRequestAuth {
            url: config.endpoint().expect("endpoint").to_string(),
            headers: vec![("authorization".into(), "Bearer test-token".into())],
        };
        let websocket = ResponsesWebSocket::new();
        let events: Vec<_> = stream(
            ResponsesExecution {
                http: &reqwest::Client::new(),
                websocket: &websocket,
                config: &config,
                auth: &auth,
            },
            plan,
            CancellationToken::new(),
        )
        .await
        .expect("stream")
        .collect()
        .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ResponseMetadata { metadata })
                if metadata.models_etag.as_deref() == Some("etag-http")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
        );
        let request = server.await.expect("server");
        assert!(request.starts_with("POST /responses HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        );
        assert!(request.contains("\"prompt_cache_key\":\"cache-key\""));
    }

    #[tokio::test]
    async fn websocket_uses_the_same_shared_mapper_and_terminal_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
            let request = socket
                .next()
                .await
                .expect("request")
                .expect("valid frame")
                .into_text()
                .expect("text frame");
            socket
                .send(WebSocketMessage::Text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_ws",
                            "status": "completed",
                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("terminal");
            request
        });
        let config = ResponsesTransportConfig::new(&format!("http://{address}/"), "responses")
            .expect("config");
        let (request, runtime) = request_and_runtime();
        let plan = prepare(
            &config,
            &agent_core::provider::Capabilities {
                reasoning: true,
                ..agent_core::provider::Capabilities::default()
            },
            request,
            |_| Ok(runtime),
            false,
            None,
        )
        .expect("plan");
        let auth = ProviderRequestAuth {
            url: config.endpoint().expect("endpoint").to_string(),
            headers: Vec::new(),
        };
        let websocket = ResponsesWebSocket::new();
        let events: Vec<_> = stream(
            ResponsesExecution {
                http: &reqwest::Client::new(),
                websocket: &websocket,
                config: &config,
                auth: &auth,
            },
            plan,
            CancellationToken::new(),
        )
        .await
        .expect("stream")
        .collect()
        .await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(StreamEvent::Done { .. })))
                .count(),
            1
        );
        let request = server.await.expect("server");
        let request: serde_json::Value = serde_json::from_str(&request).expect("request JSON");
        assert_eq!(request["model"], "gpt-5.5");
        websocket.disconnect(&config).await;
    }
}
