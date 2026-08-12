//! Validated provider policy for ChatGPT Responses requests.
//!
//! All configuration is resolved into a `reqwest::Request` before a socket is
//! opened. Header values are deliberately absent from diagnostics and `Debug`.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Duration;

use agent_auth::Secret;
use agent_auth::oauth::openai_chatgpt::CHATGPT_BASE_URL;
use agent_auth::provider::ProviderRequestAuth;
use agent_core::model::{ModelRetryPolicy, ResponsesDialect};
use agent_core::provider::{CanonicalRequest, ProviderError};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use crate::chatgpt_error::invalid_request as invalid;

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const DEFAULT_PATH: &str = "codex/responses";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_WEBSOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WEBSOCKET_WRITE_BUFFER: usize = 1024 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_QUERY_COUNT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponsesCompression {
    #[default]
    None,
    Zstd,
}

impl std::str::FromStr for ResponsesCompression {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Self::None),
            "zstd" => Ok(Self::Zstd),
            _ => Err(invalid("unsupported responses compression")),
        }
    }
}

#[derive(Clone)]
pub struct ResponsesTransportConfig {
    base_url: String,
    path: String,
    query: BTreeMap<String, String>,
    default_headers: HeaderMap,
    retry_override: Option<ModelRetryPolicy>,
    connect_timeout: Duration,
    header_timeout: Duration,
    idle_timeout: Duration,
    compression: ResponsesCompression,
    websocket_enabled: bool,
    websocket_connect_timeout: Duration,
    websocket_write_timeout: Duration,
    websocket_close_timeout: Duration,
    websocket_write_buffer: usize,
}

pub(crate) struct PreparedResponsesRequest {
    endpoint: Url,
    headers: HeaderMap,
    body: Vec<u8>,
}

pub(crate) struct PreparedResponsesRoute {
    endpoint: Url,
    headers: HeaderMap,
}

pub(crate) struct PreparedWebSocketRequest {
    pub(crate) endpoint: Url,
    pub(crate) headers: HeaderMap,
}

impl PreparedResponsesRequest {
    pub(crate) fn authorize(
        mut self,
        client: &reqwest::Client,
        auth: &ProviderRequestAuth,
    ) -> Result<reqwest::Request, ProviderError> {
        validate_authority(&self.endpoint, &auth.url)?;
        for (name, value) in auth.header_pairs() {
            if name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("content-encoding")
            {
                continue;
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("invalid credential header name"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| invalid("invalid credential header value"))?;
            self.headers.insert(name, value);
        }
        client
            .post(self.endpoint)
            .headers(self.headers)
            .body(self.body)
            .build()
            .map_err(|_| invalid("invalid responses HTTP request"))
    }
}

impl std::fmt::Debug for ResponsesTransportConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResponsesTransportConfig")
            .field("base_url", &self.base_url)
            .field("path", &self.path)
            .field("query_keys", &self.query.keys().collect::<Vec<_>>())
            .field(
                "default_header_names",
                &self.default_headers.keys().collect::<Vec<_>>(),
            )
            .field("retry_override", &self.retry_override)
            .field("connect_timeout", &self.connect_timeout)
            .field("header_timeout", &self.header_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("compression", &self.compression)
            .finish()
    }
}

impl ResponsesTransportConfig {
    pub fn new(base_url: &str, path: &str) -> Result<Self, ProviderError> {
        let mut base_url =
            Url::parse(base_url).map_err(|_| invalid("invalid responses base URL"))?;
        validate_base_url(&base_url)?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        validate_path(path)?;
        Ok(Self {
            base_url: base_url.to_string(),
            path: path.to_string(),
            query: BTreeMap::new(),
            default_headers: HeaderMap::new(),
            retry_override: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            header_timeout: DEFAULT_HEADER_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            compression: ResponsesCompression::None,
            websocket_enabled: true,
            websocket_connect_timeout: DEFAULT_WEBSOCKET_CONNECT_TIMEOUT,
            websocket_write_timeout: DEFAULT_WEBSOCKET_WRITE_TIMEOUT,
            websocket_close_timeout: DEFAULT_WEBSOCKET_CLOSE_TIMEOUT,
            websocket_write_buffer: DEFAULT_WEBSOCKET_WRITE_BUFFER,
        })
    }

    pub(crate) fn chatgpt_default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            path: DEFAULT_PATH.into(),
            query: BTreeMap::new(),
            default_headers: HeaderMap::new(),
            retry_override: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            header_timeout: DEFAULT_HEADER_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            compression: ResponsesCompression::None,
            websocket_enabled: true,
            websocket_connect_timeout: DEFAULT_WEBSOCKET_CONNECT_TIMEOUT,
            websocket_write_timeout: DEFAULT_WEBSOCKET_WRITE_TIMEOUT,
            websocket_close_timeout: DEFAULT_WEBSOCKET_CLOSE_TIMEOUT,
            websocket_write_buffer: DEFAULT_WEBSOCKET_WRITE_BUFFER,
        }
    }

    pub fn with_query(mut self, key: &str, value: &str) -> Result<Self, ProviderError> {
        if self.query.len() >= MAX_QUERY_COUNT
            || !valid_component(key, 128)
            || !valid_component(value, 1024)
        {
            return Err(invalid("invalid responses query parameter"));
        }
        self.query.insert(key.to_string(), value.to_string());
        Ok(self)
    }

    pub fn with_default_header(mut self, name: &str, value: &str) -> Result<Self, ProviderError> {
        if self.default_headers.len() >= MAX_HEADER_COUNT || forbidden_default_header(name) {
            return Err(invalid("forbidden responses default header"));
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid("invalid responses default header name"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| invalid("invalid responses default header value"))?;
        self.default_headers.insert(name, value);
        Ok(self)
    }

    pub fn with_retry(mut self, retry: ModelRetryPolicy) -> Result<Self, ProviderError> {
        if retry.max_attempts == 0 || retry.max_attempts > 16 || retry.backoff_base_ms == 0 {
            return Err(invalid("invalid responses retry policy"));
        }
        self.retry_override = Some(retry);
        Ok(self)
    }

    pub fn with_timeouts(
        mut self,
        connect: Duration,
        header: Duration,
        idle: Duration,
    ) -> Result<Self, ProviderError> {
        if connect.is_zero() || header.is_zero() || idle.is_zero() {
            return Err(invalid("responses timeouts must be nonzero"));
        }
        self.connect_timeout = connect;
        self.header_timeout = header;
        self.idle_timeout = idle;
        Ok(self)
    }

    pub fn with_compression(mut self, compression: ResponsesCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Selects the Responses WebSocket optimization for this provider session.
    /// HTTP/SSE remains the deterministic fallback when disabled or rejected.
    pub fn with_websocket(mut self, enabled: bool) -> Self {
        self.websocket_enabled = enabled;
        self
    }

    pub fn with_websocket_timeouts(
        mut self,
        connect: Duration,
        write: Duration,
        close: Duration,
    ) -> Result<Self, ProviderError> {
        if connect.is_zero() || write.is_zero() || close.is_zero() || close > Duration::from_secs(5)
        {
            return Err(invalid(
                "websocket timeouts must be nonzero and close must be at most 5 seconds",
            ));
        }
        self.websocket_connect_timeout = connect;
        self.websocket_write_timeout = write;
        self.websocket_close_timeout = close;
        Ok(self)
    }

    pub fn with_websocket_write_buffer(mut self, bytes: usize) -> Result<Self, ProviderError> {
        if !(128 * 1024..=16 * 1024 * 1024).contains(&bytes) {
            return Err(invalid(
                "websocket write buffer must contain 128 KiB..=16 MiB",
            ));
        }
        self.websocket_write_buffer = bytes;
        Ok(self)
    }

    pub fn endpoint(&self) -> Result<Url, ProviderError> {
        let base_url =
            Url::parse(&self.base_url).map_err(|_| invalid("invalid responses base URL"))?;
        let mut endpoint = base_url
            .join(&self.path)
            .map_err(|_| invalid("invalid responses endpoint"))?;
        if !self.query.is_empty() {
            let mut pairs = endpoint.query_pairs_mut();
            for (key, value) in &self.query {
                pairs.append_pair(key, value);
            }
        }
        Ok(endpoint)
    }

    /// Builds another endpoint under the same validated base URL and query
    /// policy. Configured providers use it for `/models`.
    pub(crate) fn endpoint_for_path(&self, path: &str) -> Result<Url, ProviderError> {
        validate_path(path)?;
        let base_url =
            Url::parse(&self.base_url).map_err(|_| invalid("invalid responses base URL"))?;
        let mut endpoint = base_url
            .join(path)
            .map_err(|_| invalid("invalid provider endpoint path"))?;
        if !self.query.is_empty() {
            let mut pairs = endpoint.query_pairs_mut();
            for (key, value) in &self.query {
                pairs.append_pair(key, value);
            }
        }
        Ok(endpoint)
    }

    pub(crate) fn validate_for_configured(&self) -> Result<(), ProviderError> {
        self.endpoint().map(|_| ())
    }

    /// Headers the user configured on this endpoint. `Secret` because one of
    /// them is routinely the API key of an OpenAI-compatible gateway, and they
    /// get concatenated with resolved auth headers before being sent.
    pub(crate) fn configured_endpoint_headers(
        &self,
    ) -> Result<Vec<(String, Secret)>, ProviderError> {
        self.default_headers
            .iter()
            .map(|(name, value)| {
                let value = value
                    .to_str()
                    .map_err(|_| invalid("invalid responses default header value"))?;
                Ok((name.as_str().to_string(), Secret::new(value)))
            })
            .collect()
    }

    pub fn retry_override(&self) -> Option<ModelRetryPolicy> {
        self.retry_override
    }

    pub(crate) fn effective_retry(&self, fallback: ModelRetryPolicy) -> ModelRetryPolicy {
        self.retry_override.unwrap_or(fallback)
    }

    pub(crate) fn validate_for_chatgpt(&self) -> Result<(), ProviderError> {
        self.validate_authority(CHATGPT_BASE_URL)
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn header_timeout(&self) -> Duration {
        self.header_timeout
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub(crate) fn websocket_enabled(&self) -> bool {
        self.websocket_enabled
    }

    pub(crate) fn websocket_connect_timeout(&self) -> Duration {
        self.websocket_connect_timeout
    }

    pub(crate) fn websocket_write_timeout(&self) -> Duration {
        self.websocket_write_timeout
    }

    pub(crate) fn websocket_close_timeout(&self) -> Duration {
        self.websocket_close_timeout
    }

    pub(crate) fn websocket_write_buffer(&self) -> usize {
        self.websocket_write_buffer
    }

    pub(crate) fn set_idle_timeout(&mut self, idle: Duration) {
        if !idle.is_zero() {
            self.idle_timeout = idle;
        }
    }

    #[cfg(test)]
    pub(crate) fn build_request(
        &self,
        client: &reqwest::Client,
        auth: &ProviderRequestAuth,
        canonical: &CanonicalRequest,
        dialect: ResponsesDialect,
        body: &[u8],
    ) -> Result<reqwest::Request, ProviderError> {
        self.prepare_http(self.prepare_route(canonical, dialect)?, body)?
            .authorize(client, auth)
    }

    pub(crate) fn prepare_route(
        &self,
        canonical: &CanonicalRequest,
        dialect: ResponsesDialect,
    ) -> Result<PreparedResponsesRoute, ProviderError> {
        let mut headers = self.default_headers.clone();
        for &(metadata_key, header_name) in metadata_headers() {
            if let Some(value) = canonical.client_metadata.get(metadata_key) {
                let value = HeaderValue::from_str(value)
                    .map_err(|_| invalid("invalid client metadata header value"))?;
                headers.insert(HeaderName::from_static(header_name), value);
            }
        }
        if let Some(thread_id) = canonical.client_metadata.get("thread_id") {
            let value = HeaderValue::from_str(thread_id)
                .map_err(|_| invalid("invalid client metadata thread identifier"))?;
            headers.insert(HeaderName::from_static("thread-id"), value);
        }
        if let Some(client_request_id) = canonical
            .client_metadata
            .get("client_request_id")
            .or_else(|| canonical.client_metadata.get("thread_id"))
        {
            let value = HeaderValue::from_str(client_request_id)
                .map_err(|_| invalid("invalid client request identifier"))?;
            headers.insert(HeaderName::from_static("x-client-request-id"), value);
        }
        if dialect == ResponsesDialect::Lite {
            headers.insert(
                HeaderName::from_static("x-openai-internal-codex-responses-lite"),
                HeaderValue::from_static("true"),
            );
        }
        Ok(PreparedResponsesRoute {
            endpoint: self.endpoint()?,
            headers,
        })
    }

    pub(crate) fn prepare_http(
        &self,
        route: PreparedResponsesRoute,
        body: &[u8],
    ) -> Result<PreparedResponsesRequest, ProviderError> {
        let encoded = match self.compression {
            ResponsesCompression::None => body.to_vec(),
            ResponsesCompression::Zstd => zstd::stream::encode_all(Cursor::new(body), 3)
                .map_err(|_| invalid("responses request compression failed"))?,
        };
        let PreparedResponsesRoute {
            endpoint,
            mut headers,
        } = route;
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        if self.compression == ResponsesCompression::Zstd {
            headers.insert(
                reqwest::header::CONTENT_ENCODING,
                HeaderValue::from_static("zstd"),
            );
        }
        Ok(PreparedResponsesRequest {
            endpoint,
            headers,
            body: encoded,
        })
    }

    pub(crate) fn prepare_websocket(
        &self,
        route: &PreparedResponsesRoute,
        auth: &ProviderRequestAuth,
    ) -> Result<PreparedWebSocketRequest, ProviderError> {
        let mut endpoint = route.endpoint.clone();
        let mut headers = route.headers.clone();
        validate_authority(&endpoint, &auth.url)?;
        let websocket_scheme = match endpoint.scheme() {
            "http" => "ws",
            "https" => "wss",
            _ => return Err(invalid("invalid responses websocket URL scheme")),
        };
        endpoint
            .set_scheme(websocket_scheme)
            .map_err(|_| invalid("invalid responses websocket URL"))?;

        for (name, value) in auth.header_pairs() {
            if name.eq_ignore_ascii_case("accept")
                || name.eq_ignore_ascii_case("content-type")
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("content-encoding")
                || name.eq_ignore_ascii_case("openai-beta")
            {
                continue;
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("invalid credential header name"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| invalid("invalid credential header value"))?;
            headers.insert(name, value);
        }
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses_websockets=2026-02-06"),
        );
        Ok(PreparedWebSocketRequest { endpoint, headers })
    }

    fn validate_authority(&self, authorized_url: &str) -> Result<(), ProviderError> {
        let configured = self.endpoint()?;
        validate_authority(&configured, authorized_url)
    }
}

fn validate_authority(configured: &Url, authorized_url: &str) -> Result<(), ProviderError> {
    let authorized =
        Url::parse(authorized_url).map_err(|_| invalid("invalid authorized responses URL"))?;
    if configured.origin() != authorized.origin() {
        return Err(invalid("responses endpoint origin is not authorized"));
    }
    Ok(())
}

fn metadata_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("session_id", "session-id"),
        ("originator", "originator"),
        ("beta_features", "x-codex-beta-features"),
        ("x-codex-beta-features", "x-codex-beta-features"),
        ("subagent", "x-openai-subagent"),
        ("x-openai-subagent", "x-openai-subagent"),
        ("compatibility", "x-codex-window-id"),
        ("x-codex-window-id", "x-codex-window-id"),
        ("attestation", "x-oai-attestation"),
        ("turn_state", "x-codex-turn-state"),
        ("x-codex-turn-state", "x-codex-turn-state"),
        ("turn_metadata", "x-codex-turn-metadata"),
        ("x-codex-turn-metadata", "x-codex-turn-metadata"),
        ("parent_thread_id", "x-codex-parent-thread-id"),
        ("x-codex-parent-thread-id", "x-codex-parent-thread-id"),
        ("x-codex-installation-id", "x-codex-installation-id"),
    ]
}

fn validate_base_url(url: &Url) -> Result<(), ProviderError> {
    let loopback_http = url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
    if (url.scheme() != "https" && !loopback_http)
        || url.cannot_be_a_base()
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("responses base URL is not allowed"));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), ProviderError> {
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        || !path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '-' | '.')
        })
    {
        return Err(invalid("invalid responses path"));
    }
    Ok(())
}

fn valid_component(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn forbidden_default_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "chatgpt-account-id"
            | "host"
            | "content-length"
            | "content-encoding"
            | "transfer-encoding"
            | "connection"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::ProviderErrorCategory;
    use std::collections::BTreeMap;

    fn spec(url: &str) -> ProviderRequestAuth {
        ProviderRequestAuth {
            url: url.into(),
            headers: vec![
                ("authorization".into(), "Bearer secret-token".into()),
                ("chatgpt-account-id".into(), "secret-account".into()),
                ("originator".into(), "codex_cli_rs".into()),
                ("OpenAI-Beta".into(), "responses=experimental".into()),
            ],
        }
    }

    #[test]
    fn composes_endpoint_headers_and_policy() {
        let config = ResponsesTransportConfig::new("https://example.test/api/", "responses")
            .unwrap()
            .with_query("api-version", "2026-08-01")
            .unwrap()
            .with_default_header("x-client", "pyxis")
            .unwrap()
            .with_retry(ModelRetryPolicy {
                max_attempts: 5,
                backoff_base_ms: 75,
            })
            .unwrap();
        let canonical = CanonicalRequest {
            client_metadata: BTreeMap::from([
                ("thread_id".into(), "thread-1".into()),
                ("client_request_id".into(), "request-1".into()),
                ("session_id".into(), "session-1".into()),
                ("x-openai-subagent".into(), "review".into()),
                ("attestation".into(), "attested".into()),
                ("turn_state".into(), "state-1".into()),
                ("beta_features".into(), "feature-a".into()),
                ("x-codex-window-id".into(), "window-1".into()),
            ]),
            ..CanonicalRequest::default()
        };
        let request = config
            .build_request(
                &reqwest::Client::new(),
                &spec("https://example.test/responses"),
                &canonical,
                ResponsesDialect::Lite,
                br#"{"model":"gpt-5.5"}"#,
            )
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://example.test/api/responses?api-version=2026-08-01"
        );
        assert_eq!(request.headers()["x-client"], "pyxis");
        assert_eq!(request.headers()["x-client-request-id"], "request-1");
        assert_eq!(request.headers()["thread-id"], "thread-1");
        assert_eq!(request.headers()["session-id"], "session-1");
        assert_eq!(request.headers()["x-openai-subagent"], "review");
        assert_eq!(request.headers()["x-oai-attestation"], "attested");
        assert_eq!(request.headers()["x-codex-turn-state"], "state-1");
        assert_eq!(request.headers()["originator"], "codex_cli_rs");
        assert_eq!(request.headers()["openai-beta"], "responses=experimental");
        assert_eq!(request.headers()["x-codex-beta-features"], "feature-a");
        assert_eq!(request.headers()["x-codex-window-id"], "window-1");
        assert_eq!(
            request.headers()["x-openai-internal-codex-responses-lite"],
            "true"
        );
        assert_eq!(config.retry_override().unwrap().max_attempts, 5);
        assert_eq!(
            ResponsesTransportConfig::new("https://example.test/api", "responses")
                .unwrap()
                .endpoint()
                .unwrap()
                .as_str(),
            "https://example.test/api/responses"
        );
        assert_eq!(
            ResponsesTransportConfig::chatgpt_default()
                .endpoint()
                .unwrap()
                .as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn websocket_uses_the_same_auth_scope_and_the_baseline_beta_contract() {
        let config = ResponsesTransportConfig::new("https://example.test/api/", "responses")
            .unwrap()
            .with_query("api-version", "2026-08-01")
            .unwrap()
            .with_default_header("x-tenant", "fixture-tenant")
            .unwrap();
        let canonical = CanonicalRequest {
            client_metadata: BTreeMap::from([
                ("thread_id".into(), "thread-1".into()),
                ("session_id".into(), "session-1".into()),
            ]),
            ..CanonicalRequest::default()
        };
        let route = config
            .prepare_route(&canonical, ResponsesDialect::Standard)
            .unwrap();
        let request = config
            .prepare_websocket(&route, &spec("https://example.test/responses"))
            .unwrap();
        assert_eq!(
            request.endpoint.as_str(),
            "wss://example.test/api/responses?api-version=2026-08-01"
        );
        assert_eq!(request.headers["authorization"], "Bearer secret-token");
        assert_eq!(request.headers["chatgpt-account-id"], "secret-account");
        assert_eq!(request.headers["originator"], "codex_cli_rs");
        assert_eq!(request.headers["thread-id"], "thread-1");
        assert_eq!(request.headers["session-id"], "session-1");
        assert_eq!(request.headers["x-tenant"], "fixture-tenant");
        assert_eq!(
            request.headers["openai-beta"],
            "responses_websockets=2026-02-06"
        );
        assert!(!request.headers.contains_key(reqwest::header::ACCEPT));
        assert!(!request.headers.contains_key(reqwest::header::CONTENT_TYPE));
    }

    #[test]
    fn websocket_policy_rejects_unbounded_or_overlong_close_configuration() {
        let config = ResponsesTransportConfig::new("https://example.test/", "responses").unwrap();
        assert!(
            config
                .clone()
                .with_websocket_timeouts(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(6),
                )
                .is_err()
        );
        assert!(
            config
                .clone()
                .with_websocket_write_buffer(usize::MAX)
                .is_err()
        );
        assert!(!config.with_websocket(false).websocket_enabled());
    }

    #[test]
    fn zstd_body_is_preencoded_and_exact_when_decompressed() {
        let body = br#"{"input":[{"type":"message"}]}"#;
        let request = ResponsesTransportConfig::new("https://example.test/", "responses")
            .unwrap()
            .with_compression(ResponsesCompression::Zstd)
            .build_request(
                &reqwest::Client::new(),
                &spec("https://example.test/responses"),
                &CanonicalRequest::default(),
                ResponsesDialect::Standard,
                body,
            )
            .unwrap();
        assert_eq!(request.headers()[reqwest::header::CONTENT_ENCODING], "zstd");
        let encoded = request.body().and_then(reqwest::Body::as_bytes).unwrap();
        let decoded = zstd::stream::decode_all(Cursor::new(encoded)).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn oauth_headers_are_never_forwarded_across_origins() {
        let error = ResponsesTransportConfig::new("https://example.test/", "responses")
            .unwrap()
            .build_request(
                &reqwest::Client::new(),
                &spec("https://chatgpt.com/backend-api/codex/responses"),
                &CanonicalRequest::default(),
                ResponsesDialect::Standard,
                b"{}",
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::Api {
                category: ProviderErrorCategory::InvalidRequest,
                ..
            }
        ));
        assert!(!error.to_string().contains("secret-token"));
        assert!(!error.to_string().contains("secret-account"));
    }

    #[test]
    fn invalid_configuration_is_typed_and_does_not_echo_values() {
        let secret = "secret-header-value";
        let error = ResponsesTransportConfig::new("http://example.test/", "responses").unwrap_err();
        assert!(matches!(
            error,
            ProviderError::Api {
                category: ProviderErrorCategory::InvalidRequest,
                ..
            }
        ));
        let error = ResponsesTransportConfig::new("https://example.test/", "responses")
            .unwrap()
            .with_default_header("authorization", secret)
            .unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!("brotli".parse::<ResponsesCompression>().is_err());
        assert!(ResponsesTransportConfig::new("https://example.test/", "../responses").is_err());
        assert!(
            ResponsesTransportConfig::new("https://example.test/", "responses")
                .unwrap()
                .with_retry(ModelRetryPolicy {
                    max_attempts: 0,
                    backoff_base_ms: 50,
                })
                .is_err()
        );
        assert!(
            ResponsesTransportConfig::new("https://example.test/", "responses")
                .unwrap()
                .with_timeouts(
                    Duration::ZERO,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .is_err()
        );
    }
}
