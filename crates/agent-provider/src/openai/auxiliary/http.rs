use std::time::Duration;

use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase};
use agent_core::provider::{Provider, ProviderError};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, RequestBuilder, Response};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::super::ConfiguredOpenAiProvider;

const MAX_AUXILIARY_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct AuxiliaryHttpResponse {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl ConfiguredOpenAiProvider {
    pub(crate) async fn auxiliary_json<B: Serialize + ?Sized>(
        &self,
        operation: AuxiliaryOperation,
        path: &str,
        body: &B,
    ) -> Result<AuxiliaryHttpResponse, AuxiliaryError> {
        let mut cancellation = self.cancellation_snapshot();
        self.auxiliary_json_scoped(
            operation,
            AuxiliaryPhase::Request,
            path,
            body,
            &mut cancellation,
        )
        .await
    }

    pub(crate) async fn auxiliary_json_scoped<B: Serialize + ?Sized>(
        &self,
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        path: &str,
        body: &B,
        cancellation: &mut CancellationToken,
    ) -> Result<AuxiliaryHttpResponse, AuxiliaryError> {
        let encoded = serde_json::to_vec(body).map_err(|_| {
            AuxiliaryError::invalid(operation, "body", "request is not serializable")
        })?;
        self.auxiliary_request_scoped(
            operation,
            phase,
            Method::POST,
            path,
            Some(("application/json", encoded)),
            cancellation,
        )
        .await
    }

    pub(crate) async fn auxiliary_request_scoped(
        &self,
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        method: Method,
        path: &str,
        body: Option<(&'static str, Vec<u8>)>,
        cancellation: &mut CancellationToken,
    ) -> Result<AuxiliaryHttpResponse, AuxiliaryError> {
        self.auxiliary_request_scoped_with(
            operation,
            phase,
            method,
            path,
            cancellation,
            |request| match &body {
                Some((content_type, bytes)) => request
                    .header(reqwest::header::CONTENT_TYPE, *content_type)
                    .body(bytes.clone()),
                None => request,
            },
        )
        .await
    }

    pub(crate) async fn auxiliary_request_scoped_with<F>(
        &self,
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        method: Method,
        path: &str,
        cancellation: &mut CancellationToken,
        configure: F,
    ) -> Result<AuxiliaryHttpResponse, AuxiliaryError>
    where
        F: Fn(RequestBuilder) -> RequestBuilder,
    {
        let mut recovered = false;
        loop {
            let request = self
                .auxiliary_authorized_builder(operation, phase, method.clone(), path)
                .await?;
            let request = configure(request);
            let response = self
                .send_auxiliary(operation, phase, request, cancellation)
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !recovered {
                recovered = true;
                match <ConfiguredOpenAiProvider as Provider>::refresh_auth(self).await {
                    Ok(()) => {
                        *cancellation = self.cancellation_snapshot();
                        continue;
                    }
                    Err(error) => return Err(auth_error(operation, phase, error)),
                }
            }
            return read_auxiliary_response(
                operation,
                phase,
                response,
                self.config.transport.idle_timeout(),
                cancellation.clone(),
            )
            .await;
        }
    }

    pub(crate) async fn auxiliary_authorized_builder(
        &self,
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        method: Method,
        path: &str,
    ) -> Result<RequestBuilder, AuxiliaryError> {
        let (path, query) = path
            .split_once('?')
            .map_or((path, None), |(path, query)| (path, Some(query)));
        let mut endpoint = self
            .config
            .transport
            .endpoint_for_path(path)
            .map_err(|_| AuxiliaryError::invalid(operation, "path", "invalid endpoint path"))?;
        if let Some(query) = query {
            endpoint
                .query_pairs_mut()
                .extend_pairs(url::form_urlencoded::parse(query.as_bytes()));
        }
        let (_, auth) = self
            .resolved_auth()
            .await
            .map_err(|error| auth_error(operation, phase, error))?;
        let mut request = self.http.request(method, endpoint);
        for (name, value) in self
            .config
            .transport
            .configured_endpoint_headers()
            .map_err(|_| {
                AuxiliaryError::invalid(operation, "headers", "invalid configured header")
            })?
            .into_iter()
            .chain(auth.headers().iter().cloned())
        {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                AuxiliaryError::invalid(operation, "headers", "invalid header name")
            })?;
            let value = HeaderValue::from_str(&value).map_err(|_| {
                AuxiliaryError::invalid(operation, "headers", "invalid header value")
            })?;
            request = request.header(name, value);
        }
        Ok(request)
    }

    pub(crate) async fn send_auxiliary(
        &self,
        operation: AuxiliaryOperation,
        phase: AuxiliaryPhase,
        request: RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<Response, AuxiliaryError> {
        let send = tokio::time::timeout(self.config.transport.header_timeout(), request.send());
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AuxiliaryError::Cancelled { operation, phase }),
            result = send => match result {
                Err(_) => Err(AuxiliaryError::Timeout { operation, phase }),
                Ok(Err(error)) => Err(AuxiliaryError::Transport {
                    operation,
                    phase,
                    kind: transport_kind(&error),
                }),
                Ok(Ok(response)) => Ok(response),
            },
        }
    }
}

pub(crate) async fn read_auxiliary_response(
    operation: AuxiliaryOperation,
    phase: AuxiliaryPhase,
    response: Response,
    timeout: Duration,
    cancellation: CancellationToken,
) -> Result<AuxiliaryHttpResponse, AuxiliaryError> {
    let status = response.status();
    let headers = response.headers().clone();
    if !status.is_success() {
        return Err(http_error(operation, phase, status.as_u16(), &headers));
    }
    let read = async move {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| AuxiliaryError::Transport {
                operation,
                phase,
                kind: transport_kind(&error),
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_AUXILIARY_RESPONSE_BYTES {
                return Err(AuxiliaryError::Decode {
                    operation,
                    phase: AuxiliaryPhase::Decode,
                    reason: "response exceeds 64 MiB".into(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(AuxiliaryHttpResponse { headers, body })
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AuxiliaryError::Cancelled { operation, phase }),
        result = tokio::time::timeout(timeout, read) => {
            result.map_err(|_| AuxiliaryError::Timeout { operation, phase })?
        }
    }
}

pub(crate) fn http_error(
    operation: AuxiliaryOperation,
    phase: AuxiliaryPhase,
    status: u16,
    headers: &HeaderMap,
) -> AuxiliaryError {
    AuxiliaryError::Http {
        operation,
        phase,
        status,
        request_id: bounded_header(
            headers
                .get("x-request-id")
                .or_else(|| headers.get("request-id")),
        ),
        azure_client_request_id: bounded_header(headers.get("x-ms-client-request-id")),
        azure_request_id: bounded_header(headers.get("x-ms-request-id")),
        azure_error_code: bounded_header(headers.get("x-ms-error-code")),
    }
}

fn bounded_header(value: Option<&HeaderValue>) -> Option<String> {
    value
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(str::to_string)
}

pub(crate) fn transport_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    }
}

fn auth_error(
    operation: AuxiliaryOperation,
    phase: AuxiliaryPhase,
    error: ProviderError,
) -> AuxiliaryError {
    match error {
        ProviderError::Credential(error) => AuxiliaryError::Auth {
            operation,
            phase,
            error,
        },
        _ => AuxiliaryError::Transport {
            operation,
            phase,
            kind: "configuration",
        },
    }
}
