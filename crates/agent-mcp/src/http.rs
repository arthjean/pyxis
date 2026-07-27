//! Streamable HTTP client for the MCP transport (US-013).
//!
//! `rmcp` ships the transport machinery (session, SSE reconnection, worker) but
//! leaves the HTTP calls themselves to a `StreamableHttpClient` implementation.
//! Its own implementation is gated behind a feature that pins **reqwest 0.13**,
//! while the workspace is on 0.12: enabling it would duplicate the whole
//! HTTP/TLS stack in the binary. This module implements the same trait over the
//! workspace client instead, which is the shape Codex uses as well
//! (`codex-rs/rmcp-client/src/http_client_adapter.rs`).
//!
//! Everything here is protocol plumbing (MCP 2025-06-18, Streamable HTTP):
//! `POST` carries a JSON-RPC message and answers either `202`, a JSON message or
//! an SSE stream; `GET` opens the server-to-client stream; `DELETE` ends the
//! session. The security decisions (https-only, token by env var name, untrusted
//! results) live in `config.rs` and `tool.rs`, not here.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Sse, SseStream};

const EVENT_STREAM_MIME: &str = "text/event-stream";
const JSON_MIME: &str = "application/json";
const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-ID";
/// Bound of an error body echoed back to the user: a remote server does not get
/// to flood the interface through an error message.
const MAX_ERROR_BODY: usize = 512;

type HttpResult<T> = Result<T, StreamableHttpError<reqwest::Error>>;

/// HTTP client of one remote MCP server. Cloning is cheap (`reqwest::Client` is an
/// `Arc` internally) and required by the trait: the transport clones it for every
/// SSE reconnection.
#[derive(Clone)]
pub(crate) struct StreamableHttpAdapter {
    client: reqwest::Client,
}

impl StreamableHttpAdapter {
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl StreamableHttpClient for StreamableHttpAdapter {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME, JSON_MIME].join(", "))
            .header(CONTENT_TYPE, JSON_MIME);
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = apply_custom_headers(request, custom_headers);
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id.as_ref() {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request
            .json(&message)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;

        let status = response.status();
        // 202/204: the server acknowledged without answering. Some servers answer
        // an empty 200 instead, handled below once the length is known.
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        // A 404 on an attached session means the server dropped it: the transport
        // knows how to replay `initialize` from this exact error.
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = content_type_of(&response);
        let content_length = response.content_length();
        let session_id = header_string(&response, HEADER_SESSION_ID);
        if status.is_success() && content_length == Some(0) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            return Err(unexpected_status(status, response).await);
        }
        match content_type.as_deref() {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME) => Ok(StreamableHttpPostResponse::Sse(
                SseStream::from_bytes_stream(response.bytes_stream()).boxed(),
                session_id,
            )),
            Some(ct) if ct.starts_with(JSON_MIME) => {
                let message = response.json().await.map_err(StreamableHttpError::Client)?;
                Ok(StreamableHttpPostResponse::Json(message, session_id))
            }
            other => Err(StreamableHttpError::UnexpectedContentType(
                other.map(str::to_string),
            )),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<()> {
        let mut request = self
            .client
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = apply_custom_headers(request, custom_headers);
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        // Closing a session is optional in the spec: a server that refuses the
        // verb is not a failure, the connection simply ends on our side.
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let status = response.status();
        if !status.is_success() {
            return Err(unexpected_status(status, response).await);
        }
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<BoxStream<'static, Result<Sse, SseError>>> {
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME, JSON_MIME].join(", "))
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        request = apply_custom_headers(request, custom_headers);
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        // The server-to-client stream is optional: a server that answers 405 works
        // in request/response mode only, which the transport handles.
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let status = response.status();
        if !status.is_success() {
            return Err(unexpected_status(status, response).await);
        }
        match content_type_of(&response) {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME) || ct.starts_with(JSON_MIME) => {
                Ok(SseStream::from_bytes_stream(response.bytes_stream()).boxed())
            }
            other => Err(StreamableHttpError::UnexpectedContentType(other)),
        }
    }
}

fn apply_custom_headers(
    mut request: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> reqwest::RequestBuilder {
    for (name, value) in custom_headers {
        request = request.header(name, value);
    }
    request
}

fn content_type_of(response: &reqwest::Response) -> Option<String> {
    header_string(response, CONTENT_TYPE.as_str())
}

fn header_string(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Turns a non-success response into an error that names the status and a bounded
/// excerpt of the body: without it the user only sees "transport error".
async fn unexpected_status(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> StreamableHttpError<reqwest::Error> {
    let body = response.text().await.unwrap_or_default();
    let body = truncate_chars(body.trim(), MAX_ERROR_BODY);
    let message = if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    };
    StreamableHttpError::UnexpectedServerResponse(message.into())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_are_bounded() {
        let long = "x".repeat(MAX_ERROR_BODY * 2);
        assert_eq!(
            truncate_chars(&long, MAX_ERROR_BODY).chars().count(),
            MAX_ERROR_BODY
        );
        assert_eq!(truncate_chars("short", MAX_ERROR_BODY), "short");
        // Cut on a char boundary, never in the middle of a multi-byte char.
        assert_eq!(truncate_chars("éàü", 2), "éà");
    }
}
