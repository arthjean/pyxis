mod connection;
mod protocol;
mod session;

use agent_core::auxiliary::realtime::{
    RealtimeCallRequest, RealtimeCallResponse, RealtimeSessionConfig, RealtimeWire,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase, RealtimeSession};
use agent_core::provider::{Provider, ProviderError};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::multipart::{Form, Part};
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, http};
use tokio_util::sync::CancellationToken;

use super::ConfiguredOpenAiProvider;
use connection::{OpenAiRealtimeSession, realtime_websocket_config};
use protocol::RealtimeCodec;

const MAX_SDP_BYTES: usize = 1024 * 1024;

pub(super) async fn create_call(
    provider: &ConfiguredOpenAiProvider,
    request: &RealtimeCallRequest,
) -> Result<RealtimeCallResponse, AuxiliaryError> {
    let operation = AuxiliaryOperation::RealtimeCall;
    super::ensure_supported(provider, operation)?;
    session::validate_call(&request.session)?;
    validate_sdp(&request.sdp)?;

    let route = CallRoute::new(
        request.session.wire(),
        provider.config.uses_chatgpt_backend(),
    );
    let session = session::call_value(&request.session);
    let response = match route.encoding {
        CallEncoding::Json => {
            let body = json!({"sdp": request.sdp, "session": session});
            provider
                .auxiliary_json(operation, route.path, &body)
                .await?
        }
        CallEncoding::Multipart => {
            let session = serde_json::to_string(&session)
                .map_err(|_| AuxiliaryError::invalid(operation, "session", "invalid session"))?;
            let sdp = request.sdp.clone();
            let mut cancellation = provider.cancellation_snapshot();
            provider
                .auxiliary_request_scoped_with(
                    operation,
                    AuxiliaryPhase::Request,
                    Method::POST,
                    route.path,
                    &mut cancellation,
                    move |builder| {
                        builder.multipart(
                            Form::new()
                                .part("sdp", multipart_part(sdp.clone(), "application/sdp"))
                                .part(
                                    "session",
                                    multipart_part(session.clone(), "application/json"),
                                ),
                        )
                    },
                )
                .await?
        }
    };

    let sdp = String::from_utf8(response.body)
        .map_err(|_| AuxiliaryError::decode(operation, "invalid SDP response"))?;
    validate_sdp(&sdp)?;
    let location = response
        .headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing Location header"))?;
    let call_id = call_id_from_location(location)
        .ok_or_else(|| AuxiliaryError::decode(operation, "Location has no call id"))?;
    Ok(RealtimeCallResponse { sdp, call_id })
}

pub(super) async fn connect(
    provider: &ConfiguredOpenAiProvider,
    config: RealtimeSessionConfig,
    call_id: Option<&str>,
) -> Result<Box<dyn RealtimeSession>, AuxiliaryError> {
    let operation = AuxiliaryOperation::RealtimeWebSocket;
    super::ensure_supported(provider, operation)?;
    session::validate(&config)?;
    if let Some(call_id) = call_id {
        validate_call_id(call_id)?;
    }

    let codec = RealtimeCodec::for_config(&config);
    let mut endpoint = provider
        .config
        .transport
        .endpoint_for_path(&realtime_path(codec.wire(), call_id))
        .map_err(|_| AuxiliaryError::invalid(operation, "path", "invalid realtime path"))?;
    {
        let mut query = endpoint.query_pairs_mut();
        if call_id.is_none()
            && let Some(model) = config.model()
        {
            query.append_pair("model", model);
        }
        if codec.wire() == RealtimeWire::V1 {
            query.append_pair("intent", "quicksilver");
        }
        if codec.wire() != RealtimeWire::FramelessBidi
            && let Some(call_id) = call_id
        {
            query.append_pair("call_id", call_id);
        }
    }
    let scheme = match endpoint.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => {
            return Err(AuxiliaryError::invalid(
                operation,
                "url",
                "invalid URL scheme",
            ));
        }
    };
    endpoint
        .set_scheme(scheme)
        .map_err(|_| AuxiliaryError::invalid(operation, "url", "invalid WebSocket URL"))?;

    let (socket, cancellation) = connect_socket(provider, &endpoint, config.session_id()).await?;
    let connection = OpenAiRealtimeSession::new(
        socket,
        codec,
        cancellation,
        provider.config.transport.websocket_write_timeout(),
        provider.config.transport.idle_timeout(),
        provider.config.transport.websocket_close_timeout(),
    );
    let initialize = call_id.is_none() || codec.wire() != RealtimeWire::FramelessBidi;
    if initialize {
        connection.send_initial_session(&config).await?;
    }
    if call_id.is_none() && codec.wire() == RealtimeWire::FramelessBidi {
        connection.confirm_frameless_started().await?;
    }
    Ok(Box::new(connection))
}

async fn connect_socket(
    provider: &ConfiguredOpenAiProvider,
    endpoint: &url::Url,
    session_id: Option<&str>,
) -> Result<(connection::Socket, CancellationToken), AuxiliaryError> {
    let operation = AuxiliaryOperation::RealtimeWebSocket;
    let mut recovered = false;
    let mut cancellation = provider.cancellation_snapshot();
    loop {
        let mut request = endpoint
            .as_str()
            .into_client_request()
            .map_err(|_| AuxiliaryError::invalid(operation, "url", "invalid WebSocket URL"))?;
        let (_, auth) = provider
            .resolved_auth()
            .await
            .map_err(realtime_auth_error)?;
        for (name, value) in provider
            .config
            .transport
            .configured_endpoint_headers()
            .map_err(|_| AuxiliaryError::invalid(operation, "headers", "invalid headers"))?
            .into_iter()
            .chain(auth.headers().iter().cloned())
        {
            let name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AuxiliaryError::invalid(operation, "headers", "invalid name"))?;
            let value = http::HeaderValue::from_str(&value)
                .map_err(|_| AuxiliaryError::invalid(operation, "headers", "invalid value"))?;
            request.headers_mut().insert(name, value);
        }
        if let Some(session_id) = session_id {
            let value = http::HeaderValue::from_str(session_id).map_err(|_| {
                AuxiliaryError::invalid(operation, "session_id", "invalid header value")
            })?;
            request.headers_mut().insert("x-session-id", value);
        }
        let connect = tokio::time::timeout(
            provider.config.transport.websocket_connect_timeout(),
            tokio_tungstenite::connect_async_with_config(
                request,
                Some(realtime_websocket_config(
                    provider.config.transport.websocket_write_buffer(),
                )),
                false,
            ),
        );
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AuxiliaryError::Cancelled {
                    operation,
                    phase: AuxiliaryPhase::Connect,
                });
            }
            result = connect => result,
        };
        match result {
            Err(_) => {
                return Err(AuxiliaryError::Timeout {
                    operation,
                    phase: AuxiliaryPhase::Connect,
                });
            }
            Ok(Ok((socket, _))) => return Ok((socket, cancellation)),
            Ok(Err(WebSocketError::Http(response)))
                if response.status().as_u16() == 401 && !recovered =>
            {
                recovered = true;
                <ConfiguredOpenAiProvider as Provider>::refresh_auth(provider)
                    .await
                    .map_err(realtime_auth_error)?;
                cancellation = provider.cancellation_snapshot();
            }
            Ok(Err(error)) => {
                return Err(connection::map_websocket_error(
                    error,
                    AuxiliaryPhase::Connect,
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CallRoute {
    path: &'static str,
    encoding: CallEncoding,
}

impl CallRoute {
    fn new(wire: RealtimeWire, chatgpt_backend: bool) -> Self {
        match (wire, chatgpt_backend) {
            (RealtimeWire::FramelessBidi, false) => Self {
                path: "live",
                encoding: CallEncoding::Multipart,
            },
            (_, false) => Self {
                path: "realtime/calls?intent=quicksilver&architecture=avas",
                encoding: CallEncoding::Multipart,
            },
            (_, true) => Self {
                path: "realtime/calls?intent=quicksilver&architecture=avas",
                encoding: CallEncoding::Json,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CallEncoding {
    Json,
    Multipart,
}

fn multipart_part(value: String, content_type: &'static str) -> Part {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    Part::text(value).headers(headers)
}

fn realtime_path(wire: RealtimeWire, call_id: Option<&str>) -> String {
    match (wire, call_id) {
        (RealtimeWire::FramelessBidi, Some(call_id)) => format!("live/{call_id}"),
        (RealtimeWire::FramelessBidi, None) => "live".into(),
        (RealtimeWire::V1 | RealtimeWire::RealtimeV2, _) => "realtime".into(),
    }
}

fn validate_sdp(sdp: &str) -> Result<(), AuxiliaryError> {
    if sdp.is_empty()
        || sdp.len() > MAX_SDP_BYTES
        || sdp.contains('\0')
        || !sdp.lines().next().is_some_and(|line| line.trim() == "v=0")
    {
        return Err(AuxiliaryError::invalid(
            AuxiliaryOperation::RealtimeCall,
            "sdp",
            "expected a bounded SDP document starting with v=0",
        ));
    }
    Ok(())
}

fn validate_call_id(call_id: &str) -> Result<(), AuxiliaryError> {
    if call_id.is_empty()
        || call_id.len() > 256
        || !call_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(AuxiliaryError::invalid(
            AuxiliaryOperation::RealtimeWebSocket,
            "call_id",
            "invalid call identifier",
        ));
    }
    Ok(())
}

fn call_id_from_location(location: &str) -> Option<String> {
    location
        .split('?')
        .next()
        .unwrap_or(location)
        .rsplit('/')
        .find(|segment| {
            (segment.starts_with("rtc_") && (5..=256).contains(&segment.len())) || is_uuid(segment)
        })
        .map(str::to_string)
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(index, character)| match index {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

fn realtime_auth_error(error: ProviderError) -> AuxiliaryError {
    match error {
        ProviderError::Credential(error) => AuxiliaryError::Auth {
            operation: AuxiliaryOperation::RealtimeWebSocket,
            phase: AuxiliaryPhase::Connect,
            error,
        },
        _ => AuxiliaryError::Transport {
            operation: AuxiliaryOperation::RealtimeWebSocket,
            phase: AuxiliaryPhase::Connect,
            kind: "configuration",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_route_is_selected_by_explicit_provider_scope() {
        assert!(matches!(
            CallRoute::new(RealtimeWire::V1, false),
            CallRoute {
                path: "realtime/calls?intent=quicksilver&architecture=avas",
                encoding: CallEncoding::Multipart,
            }
        ));
        assert!(matches!(
            CallRoute::new(RealtimeWire::FramelessBidi, false),
            CallRoute {
                path: "live",
                encoding: CallEncoding::Multipart,
            }
        ));
        assert!(matches!(
            CallRoute::new(RealtimeWire::FramelessBidi, true),
            CallRoute {
                path: "realtime/calls?intent=quicksilver&architecture=avas",
                encoding: CallEncoding::Json,
            }
        ));
    }
}
