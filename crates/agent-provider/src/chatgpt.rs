//! `OpenAiChatGpt` adapter: ChatGPT subscription through the Responses API on the
//! ChatGPT/Codex backend (ADR-10). Implements `agent_core::Provider`.
//!
//! **Stateless SSE**: `server_side_state = false` -> no `previous_response_id`,
//! full context rebuilt on the client side every turn -> maps cleanly onto the
//! canonical model (PROVIDERS 4.1, the WebSocket+state trap is explicitly avoided).
//!
//! The backend can receive `include: ["reasoning.encrypted_content"]` when
//! reasoning replay is explicitly enabled. By default, that path stays OFF
//! until the post-rename wire format is validated live.

use std::sync::RwLock;
use std::time::Duration;

use agent_auth::OAuthCredential;
use agent_core::message::ContentBlock;
use agent_core::model::{
    ModelRetryPolicy, ModelRuntimeError, ResolvedModelRuntime, ResponsesDialect,
};
use agent_core::provider::{
    AuthError, CacheCapabilities, CanonicalRequest, CanonicalResponse, Capabilities,
    CapabilityLimits, ErrorClass, Provider, ProviderError, ProviderKind, ReasoningCapabilities,
    StopReason, StreamEvent, TokenUsage, ToolCallingCapabilities,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_request::{ResponsesBodyOptions, build_responses_body, inject_cache_key};
use crate::credential::CredentialManager;
use crate::models::ModelCatalog;

/// Keyring key of the ChatGPT subscription credential (rotating refresh rewritten here).
pub const KEYRING_ACCOUNT: &str = "oauth:openai_chatgpt";

/// Default context window (GPT-5.x models of the Codex backend). **Volatile
/// value, to be adjusted**: it ONLY affects the compaction thresholds; a real
/// overflow triggers reactive compaction (413, withholding). Conservative.
pub const DEFAULT_MAX_CONTEXT: u32 = 256_000;

/// Default reasoning effort (Codex CLI is roughly "medium").
pub const DEFAULT_REASONING_EFFORT: &str = "medium";

/// Default model slug. The Codex backend (ChatGPT subscription) enforces an
/// allow-list of VERSIONED slugs that it keeps changing (frequent removals): the
/// generic `gpt-5` slug is rejected with a 400 ("not supported when using Codex with
/// a ChatGPT account"). **Volatile value**, overridable through `--model` or the
/// `/models` command in session (see `agent_tui::MODELS`).
pub const DEFAULT_MODEL: &str = "gpt-5.5";

fn reasoning_effort_for_request(effort: &str) -> &str {
    if effort.eq_ignore_ascii_case("ultra") {
        "max"
    } else {
        effort
    }
}

/// Bound of the captured HTTP error body (avoids a giant message in the logs).
const MAX_ERR_BODY: usize = 2000;

/// Total budget of the catalog discovery (`/models`). Off the critical path:
/// a slow backend must never delay the session, the bundled catalog takes
/// over.
const MODELS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

/// Connection ESTABLISHMENT timeout (US-022). A backend that never establishes
/// the connection (corporate proxy, blackholed DNS) fails here rather than freezing;
/// the `reqwest` error is mapped to `Transport` -> classified `Retryable`. Pi: 20 s.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Default per-event idle timeout (US-022). An OPEN SSE stream that emits
/// no more events (silent backend, queue) is cancelled after this delay ->
/// `Stream("idle timeout")` (Retryable). Configurable per session (`with_idle_timeout`,
/// env `PYXIS_IDLE_TIMEOUT_SECS`). Pi: 20 s (header); Codex CLI: 300 s/event.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct OpenAiChatGptProvider {
    creds: CredentialManager,
    http: reqwest::Client,
    capabilities: Capabilities,
    reasoning_effort: Option<String>,
    /// Max delay without an SSE event before cancelling (US-022). See `DEFAULT_IDLE_TIMEOUT`.
    idle_timeout: Duration,
    /// STABLE session identifier (UUID v4), sent as `prompt_cache_key` on
    /// every request (US-029) -> the backend reuses its prefix cache.
    session_id: RwLock<String>,
    /// Last valid remote catalog layered over the versioned embedded fallback.
    /// Shared by interactive discovery and headless turn resolution.
    catalog: RwLock<ModelCatalog>,
}

/// Generates a UUID v4 (RFC 4122) from 16 random bytes. Avoids the `uuid` crate
/// (reuses `rand`, already in the workspace).
fn new_session_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

impl OpenAiChatGptProvider {
    /// Builds the adapter from an already loaded OAuth credential (by the CLI,
    /// from the keyring). `max_context` drives the compaction; `reasoning_effort`
    /// = `None` omits the `reasoning` field.
    pub fn new(cred: OAuthCredential, max_context: u32, reasoning_effort: Option<String>) -> Self {
        // US-022: `connect_timeout` bounds the TCP/TLS establishment. A `build()`
        // failure (TLS backend unavailable) falls back on the default client:
        // never a panic (`panic = deny` lint).
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let creds = CredentialManager::new(cred, http.clone(), KEYRING_ACCOUNT);
        Self {
            creds,
            http,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                // implicit caching on the backend side, not explicitly controlled.
                prompt_caching: false,
                reasoning: true,
                // KEY: stateless SSE -> the client-side canonical model maps (PROVIDERS 4.1).
                server_side_state: false,
                max_context,
                limits: CapabilityLimits {
                    max_images_per_request: None,
                    max_tool_schema_bytes: Some(64 * 1024),
                },
                tool_calling: ToolCallingCapabilities {
                    parallel_tool_calls: true,
                    strict_json_schema: true,
                    // The Responses wire carries `type: "custom"` tools, so a
                    // freeform plan reaches the backend intact.
                    freeform_tools: true,
                },
                reasoning_options: ReasoningCapabilities {
                    encrypted_replay: true,
                },
                cache: CacheCapabilities {
                    prompt_cache_key: true,
                },
            },
            reasoning_effort,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            session_id: RwLock::new(new_session_id()),
            catalog: RwLock::new(ModelCatalog::embedded()),
        }
    }

    /// Convenience constructor: MVP defaults (`DEFAULT_MAX_CONTEXT`, medium effort).
    pub fn from_credential(cred: OAuthCredential) -> Self {
        Self::new(
            cred,
            DEFAULT_MAX_CONTEXT,
            Some(DEFAULT_REASONING_EFFORT.to_string()),
        )
    }

    /// Overrides the SSE idle timeout (US-022). `Duration::ZERO` is ignored (keeps
    /// the default) so that an absurd env value does not disable the watchdog.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        if !idle.is_zero() {
            self.idle_timeout = idle;
        }
        self
    }

    pub fn with_prompt_cache_key(self, key: impl Into<String>) -> Self {
        if let Ok(mut session_id) = self.session_id.write() {
            *session_id = key.into();
        }
        self
    }

    fn prompt_cache_key(&self) -> String {
        self.session_id
            .read()
            .map(|key| key.clone())
            .unwrap_or_else(|_| new_session_id())
    }

    /// Catalog of the models offered to the connected account (`GET /models`), sorted by
    /// backend priority. Discovered at runtime: never a list frozen in the
    /// binary (the backend removes/adds slugs without notice). Outside the
    /// `Provider` trait: the notion of a catalog is specific to this adapter.
    pub async fn list_models(&self) -> Result<Vec<crate::models::CatalogModel>, ProviderError> {
        let spec = self.creds.models_spec().await?;
        let mut req = self.http.get(&spec.url).timeout(MODELS_TIMEOUT);
        for (name, value) in &spec.headers {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("models: {e}")))?;
        let status = resp.status();
        let mut body_bytes = Vec::new();
        let mut body_stream = resp.bytes_stream();
        while let Some(chunk) = body_stream.next().await {
            let chunk =
                chunk.map_err(|error| ProviderError::Transport(format!("models body: {error}")))?;
            if body_bytes.len().saturating_add(chunk.len()) > MAX_MODELS_BODY {
                return Err(ProviderError::Decode(format!(
                    "models: response exceeds {MAX_MODELS_BODY} bytes"
                )));
            }
            body_bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body_bytes)
            .map_err(|error| ProviderError::Decode(format!("models: invalid UTF-8: {error}")))?;
        if !status.is_success() {
            let message = truncate_chars(&body, MAX_ERR_BODY);
            return Err(ProviderError::Http {
                status: status.as_u16(),
                message,
                retry_after_ms: None,
            });
        }
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let mut catalog = self
            .catalog
            .write()
            .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))?;
        let models = catalog
            .install_remote(&body, &fetched_at)
            .map_err(|error| ProviderError::Decode(format!("models: {error}")))?;
        for diagnostic in catalog.diagnostics() {
            tracing::warn!(
                target: "pyxis::models",
                diagnostic,
                "remote model descriptor was not used"
            );
        }
        Ok(models)
    }

    fn catalog_window(&self, model: &str) -> Option<u32> {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.context_window(model.trim()))
    }
}

/// SSE watchdog (US-022): wraps a canonical event stream in a per-event
/// timeout. As long as an event arrives before `idle`, it is relayed as is; a
/// silence > `idle` (frozen backend) cuts the stream with `Stream("idle timeout")`
/// (classified `Retryable` -> the agent loop retries/gives up, never freezes). An
/// upstream error is relayed then ends the stream (parity with the direct path).
fn idle_guarded<S>(
    mut inner: S,
    idle: Duration,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Send + Unpin + 'static,
{
    async_stream::stream! {
        loop {
            match tokio::time::timeout(idle, inner.next()).await {
                // no event since `idle` -> silent backend.
                Err(_elapsed) => {
                    yield Err(ProviderError::Stream("idle timeout".to_string()));
                    return;
                }
                Ok(None) => break, // normal end of stream.
                Ok(Some(item)) => {
                    let stop = item.is_err();
                    yield item;
                    if stop {
                        return;
                    }
                }
            }
        }
    }
}

/// Bounds the HEADERS phase of a request (US-022 hardening). `reqwest::send()`
/// resolves when the response headers are received -> this timeout does NOT cut the
/// long SSE stream that follows (covered separately by `idle_guarded`). An overrun
/// (`Elapsed`) becomes `Stream("header timeout")` -> classified `Retryable`, parity
/// with the idle timeout. A network error from `send()` stays `Transport` (Retryable).
async fn send_with_header_timeout(
    rb: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    tokio::time::timeout(timeout, rb.send())
        .await
        .map_err(|_elapsed| ProviderError::Stream("header timeout".to_string()))?
        .map_err(|e| ProviderError::Transport(e.to_string()))
}

async fn http_error_from_response(resp: reqwest::Response) -> ProviderError {
    let code = resp.status().as_u16();
    if code == 413 {
        return ProviderError::ContextLengthExceeded;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let retry_after_ms = parse_retry_after_ms(resp.headers(), now_ms);
    let quota = crate::quota::parse_quota_headers(resp.headers());
    let mut text = sanitize_error_body(&resp.text().await.unwrap_or_default());
    // Truncated BEFORE prefixing: the terminal-quota markers sit at the start of
    // the body, and `is_terminal_rate_limit` must keep seeing them.
    text = truncate_chars(&text, MAX_ERR_BODY);
    // US-003 AC3: an exhausted quota is announced by naming the limit and its
    // reset instead of handing back the raw body. The body stays appended: it
    // carries the markers used for the terminal/transient classification.
    if code == 429 && is_terminal_rate_limit(&text) {
        text = format!(
            "{} {text}",
            crate::quota::quota_refusal_message(quota.as_ref())
        );
    }
    ProviderError::Http {
        status: code,
        message: text,
        retry_after_ms,
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn should_retry_without_reasoning_replay(status: u16, message: &str) -> bool {
    if status != 400 {
        return false;
    }
    let message = message.to_ascii_lowercase();
    message.contains("encrypted_reasoning")
        || message.contains("encrypted reasoning")
        || message.contains("reasoning replay")
}

/// Markers of a TERMINAL 429 (subscription quota exhausted), derived from Pi
/// (`isTerminalRateLimitError`). A 429 carrying one of them must NEVER be
/// retried: the session neither burns its attempts nor harasses a blocked account
/// (US-023, edge case #2). ASCII lowercase comparison (the body is backend error
/// JSON).
///
/// We only keep the UNAMBIGUOUS signals: Pi's bare substrings `"billing"`
/// and `"available balance"` are ruled out (a transient 429 saying "rate limited;
/// see billing dashboard" would be wrongly dropped). Deliberate bias toward false negatives:
/// a missed terminal falls back to `RateLimited` -> BOUNDED retry (`max_attempts` + 60 s cap),
/// a safe degradation; a false positive would kill the session with no recourse.
const TERMINAL_RATE_LIMIT_MARKERS: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
];

/// True when the body of a 429 denotes an exhausted quota (terminal), not a transient
/// overload. See `TERMINAL_RATE_LIMIT_MARKERS`.
pub fn is_terminal_rate_limit(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    TERMINAL_RATE_LIMIT_MARKERS
        .iter()
        .any(|m| lower.contains(m))
}

fn redact_bearer_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.to_ascii_lowercase().find("bearer ") {
        let (before, after_before) = rest.split_at(pos);
        out.push_str(before);
        out.push_str("Bearer [redacted]");
        let value = &after_before["bearer ".len()..];
        let end = value
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '}' | ']'))
            .unwrap_or(value.len());
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

fn redact_form_value(input: &str, key: &str) -> String {
    let marker = format!("{key}=");
    let lower_marker = marker.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.to_ascii_lowercase().find(&lower_marker) {
        let (before, after_before) = rest.split_at(pos);
        out.push_str(before);
        out.push_str(&after_before[..marker.len()]);
        out.push_str("[redacted]");
        let value = &after_before[marker.len()..];
        let end = value
            .find(|c: char| c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';'))
            .unwrap_or(value.len());
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

fn sensitive_json_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("secret")
        || lower.contains("password")
        || lower == "api_key"
        || lower == "apikey"
        || lower == "chatgpt-account-id"
        || lower == "chatgpt_account_id"
        || lower == "account_id"
}

fn redact_json_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if sensitive_json_key(key) {
                    *value = serde_json::Value::String("[redacted]".to_string());
                } else {
                    redact_json_secrets(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_secrets(item);
            }
        }
        _ => {}
    }
}

fn sanitize_error_body(body: &str) -> String {
    let mut text = if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) {
        redact_json_secrets(&mut value);
        serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
    } else {
        body.to_string()
    };
    text = redact_bearer_tokens(&text);
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "authorization",
        "chatgpt-account-id",
        "chatgpt_account_id",
        "api_key",
    ] {
        text = redact_form_value(&text, key);
    }
    text
}

/// Parses the server delay of a `Retry-After` into milliseconds (US-023), in
/// Pi's priority order: `retry-after-ms` (exact ms) > `Retry-After`
/// (whole seconds) > `Retry-After` (IMF-fixdate HTTP date, delta vs `now_ms`).
/// `now_ms` is injected for testability. `None` when no header is usable.
///
/// NB: the RETURNED value is NOT capped here (it reflects the raw server value);
/// the `MAX_RETRY_AFTER_MS` cap is applied by the consumer (`retry_delay`,
/// agent-core). Any future consumer of `retry_after_ms` must re-bound it.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap, now_ms: u64) -> Option<u64> {
    // 1) `retry-after-ms`: exact milliseconds, takes priority.
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite())
    {
        return Some(ms.max(0.0) as u64);
    }
    // 2) `Retry-After`: whole seconds, otherwise an HTTP date.
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Some(secs) = raw.parse::<f64>().ok().filter(|s| s.is_finite()) {
        return Some((secs.max(0.0) * 1000.0) as u64);
    }
    // 3) absolute HTTP date -> positive delta until the deadline.
    let target_ms = parse_imf_fixdate_ms(raw)?;
    Some(target_ms.saturating_sub(now_ms))
}

/// Parses an IMF-fixdate HTTP date (`"Tue, 15 Nov 1994 08:12:31 GMT"`, RFC 7231)
/// into epoch ms. The only format emitted by the OpenAI backends; the legacy variants
/// (RFC 850 / asctime) are not handled (returns `None`). Avoids an external date
/// dependency (offline-safe).
fn parse_imf_fixdate_ms(s: &str) -> Option<u64> {
    let rest = s.trim().split_once(", ")?.1; // "15 Nov 1994 08:12:31 GMT"
    let mut it = rest.split(' ');
    let day: i64 = it.next()?.parse().ok()?;
    let month: i64 = match it.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next()?.parse().ok()?;
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3_600 + mi * 60 + se;
    if secs < 0 {
        return None;
    }
    Some((secs as u64) * 1000)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm, public domain). Exact
/// over the whole proleptic Gregorian range.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[async_trait]
impl Provider for OpenAiChatGptProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn max_context_for_model(&self, model: &str) -> u32 {
        self.catalog_window(model)
            .unwrap_or(self.capabilities.max_context)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.catalog_window(model)
    }

    fn resolve_model_runtime(
        &self,
        model: &str,
        reasoning_effort: Option<&str>,
        max_output_tokens: u32,
        max_retries: u32,
        backoff_base_ms: u64,
    ) -> Result<ResolvedModelRuntime, ModelRuntimeError> {
        let catalog = self
            .catalog
            .read()
            .map_err(|_| ModelRuntimeError::InvalidField {
                field: "catalog",
                detail: "lock poisoned".into(),
            })?;
        catalog.resolve(
            model,
            reasoning_effort.or(self.reasoning_effort.as_deref()),
            max_output_tokens,
            ModelRetryPolicy {
                max_attempts: max_retries.saturating_add(1),
                backoff_base_ms,
            },
        )
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        req.validate()
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        // A tool this adapter cannot serialize is refused here, before the
        // credential is read and before any socket is opened.
        self.capabilities.ensure_tools_supported(&req.tools)?;
        let runtime = match req.model_runtime.clone() {
            Some(runtime) => runtime,
            None => self
                .resolve_model_runtime(
                    &req.model,
                    req.reasoning_effort.as_deref(),
                    req.max_output_tokens,
                    3,
                    50,
                )
                .map_err(|error| ProviderError::Decode(error.to_string()))?,
        };
        let mut req = req;
        if req.model_runtime.is_none() {
            req.reasoning_effort = runtime.reasoning_effort.clone();
            req.model_runtime = Some(runtime.clone());
        }
        req.validate()
            .map_err(|error| ProviderError::Decode(error.to_string()))?;

        // Credential access and every network operation happen only after the
        // complete effective request has passed canonical validation.
        let spec = self.creds.request_spec().await?;
        let reasoning_effort = runtime
            .reasoning_effort
            .as_deref()
            .map(reasoning_effort_for_request);
        let mut body = build_responses_body(
            &req,
            ResponsesBodyOptions {
                reasoning_effort,
                include_encrypted_reasoning: req.reasoning_replay
                    && runtime.reasoning_effort.is_some(),
                parallel_tool_calls: runtime.supports_parallel_tool_calls,
                text_verbosity: if runtime.supports_verbosity {
                    runtime.verbosity.as_deref()
                } else {
                    None
                },
                dialect: runtime.responses_dialect,
            },
        );
        // US-029: stable per-session cache key -> reuse of the backend cache.
        let prompt_cache_key = self.prompt_cache_key();
        inject_cache_key(&mut body, &prompt_cache_key);

        // 3. POST. `.json()` sets the content-type; we add the proprietary headers
        //    (Authorization, chatgpt-account-id, originator, OpenAI-Beta, accept).
        let build_request = |body: &serde_json::Value| {
            let mut rb = self.http.post(&spec.url).json(body);
            for (k, v) in &spec.headers {
                if k.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                rb = rb.header(k, v);
            }
            if runtime.responses_dialect == ResponsesDialect::Lite {
                rb = rb.header("x-openai-internal-codex-responses-lite", "true");
            }
            rb
        };
        // US-022 (hardening): bounds the HEADERS phase. `connect_timeout` covers
        // the TCP/TLS establishment and `idle_guarded` the OPEN stream, but between the
        // two `send()` waits for the response headers without a bound: a backend that
        // handshakes then withholds its headers (blocked proxy, queue) would freeze the loop
        // without a signal. `send()` resolves when the headers are received -> this timeout
        // does NOT cut the long SSE stream that follows.
        let resp = send_with_header_timeout(build_request(&body), CONNECT_TIMEOUT).await?;

        // 4. status. 413 -> context error (withholding/reactive compaction).
        if !resp.status().is_success() {
            return Err(http_error_from_response(resp).await);
        }

        // 5. SSE stream -> canonical StreamEvent (never ANSI, never a panic).
        //    Stateful mapping (one SSE event -> 0..n StreamEvent) in an async_stream,
        //    then the `idle_guarded` watchdog: the timeout wraps `inner.next()`, so
        //    an `es.next()` that stalls (mute backend) triggers the idle timeout, without
        //    cutting while draining already buffered events (US-022).
        //    US-003: the quota state travels in the response headers, so it is read
        //    here, before the body is consumed, and emitted first.
        let quota = crate::quota::parse_quota_headers(resp.headers());
        let mut es = resp.bytes_stream().eventsource();
        let mapped = async_stream::stream! {
            if let Some(snapshot) = quota {
                yield Ok(StreamEvent::Quota { snapshot });
            }
            let mut mapper = CodexEventMapper::with_replay(req.reasoning_replay);
            let mut saw_terminal = false;
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(event) => match mapper.ingest(&event.data) {
                        Ok(events) => {
                            for e in events {
                                if matches!(e, StreamEvent::Done { .. }) {
                                    saw_terminal = true;
                                }
                                yield Ok(e);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    },
                    Err(e) => {
                        yield Err(ProviderError::Stream(e.to_string()));
                        return;
                    }
                }
            }
            if !saw_terminal {
                yield Err(ProviderError::Stream("missing terminal event".to_string()));
            }
        };
        Ok(idle_guarded(mapped.boxed(), self.idle_timeout).boxed())
    }

    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        // Reuses the stream path and aggregates (titles / compaction summaries).
        let stream = self.stream(req).await?;
        futures_util::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let mut stop = StopReason::EndTurn;
        while let Some(ev) = stream.next().await {
            match ev? {
                StreamEvent::TextDelta { text: t } => text.push_str(&t),
                StreamEvent::Usage { usage: u } => usage = u,
                StreamEvent::Done { stop: s } => stop = s,
                _ => {}
            }
        }
        Ok(CanonicalResponse {
            content: vec![ContentBlock::Text { text }],
            usage,
            stop,
        })
    }

    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        self.creds.force_refresh().await
    }

    async fn disconnect_auth(&self) -> Result<(), ProviderError> {
        self.creds.disconnect().await;
        Ok(())
    }

    fn set_prompt_cache_key(&self, key: &str) {
        if let Ok(mut session_id) = self.session_id.write() {
            *session_id = key.to_string();
        }
    }

    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        match err {
            ProviderError::Credential(error) => ErrorClass::Auth(*error),
            ProviderError::Http {
                status, message, ..
            } => match *status {
                // This adapter owns a refresh-capable credential manager. A 401
                // therefore authorizes one sampling-scoped recovery regardless
                // of backend body wording; the core enforces the bound.
                401 => ErrorClass::Auth(AuthError::Expired),
                403 => ErrorClass::Auth(AuthError::Invalid),
                400 if should_retry_without_reasoning_replay(*status, message) => {
                    ErrorClass::ReasoningReplayRejected
                }
                // 429 with an exhausted quota (GoUsageLimitError/billing/... body) -> TERMINAL:
                // never retried (US-023). A transient 429 stays `RateLimited`.
                429 if is_terminal_rate_limit(message) => ErrorClass::InvalidRequest,
                429 => ErrorClass::RateLimited,
                529 => ErrorClass::Overloaded(529),
                s if s >= 500 => ErrorClass::Retryable,
                _ => ErrorClass::InvalidRequest,
            },
            // Transient: transport, cut stream, garbled chunk -> cross-cutting retry.
            ProviderError::Transport(_) | ProviderError::Stream(_) | ProviderError::Decode(_) => {
                ErrorClass::Retryable
            }
            // Does not reach classify (is_context_error handled upstream); fail-safe.
            ProviderError::ContextLengthExceeded => ErrorClass::InvalidRequest,
            // Decided locally before any request: retrying cannot change it.
            ProviderError::UnsupportedTool { .. } => ErrorClass::InvalidRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> OpenAiChatGptProvider {
        OpenAiChatGptProvider::new(
            OAuthCredential {
                provider: agent_auth::ProviderId::OpenAiChatGpt,
                access: agent_auth::Secret::new("AT"),
                refresh: agent_auth::Secret::new("RT"),
                expires_at: u64::MAX,
                account_id: Some("acct".into()),
            },
            DEFAULT_MAX_CONTEXT,
            None,
        )
    }

    #[test]
    fn capabilities_are_sse_stateless() {
        let p = provider();
        let c = p.capabilities();
        assert!(!c.server_side_state, "SSE stateless → mappe le canonique");
        assert!(c.tools && c.reasoning);
        assert!(c.reasoning_options.encrypted_replay);
        assert_eq!(p.kind(), ProviderKind::OpenAiChatGpt);
    }

    #[tokio::test]
    async fn direct_stream_validates_before_credentials_or_network() {
        let p = provider();
        let request = CanonicalRequest {
            model: String::new(),
            model_runtime: None,
            reasoning_effort: None,
            reasoning_replay: false,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: 4096,
        };
        assert!(
            matches!(
                p.stream(request).await,
                Err(ProviderError::Decode(ref detail)) if detail.contains("model is empty")
            ),
            "invalid request must fail before opening a stream"
        );
    }

    #[test]
    fn encrypted_reasoning_transport_is_available_but_descriptor_gated() {
        let p = provider();
        assert!(p.capabilities().reasoning_options.encrypted_replay);
    }

    #[test]
    fn resolved_runtime_controls_context_and_features() {
        let p = provider();
        let runtime = p
            .resolve_model_runtime("gpt-5.5", Some("high"), 4096, 3, 50)
            .expect("embedded descriptor resolves");
        assert_eq!(runtime.context_window, 272_000);
        assert!(runtime.supports_parallel_tool_calls);
        assert_eq!(runtime.reasoning_effort.as_deref(), Some("high"));
        assert!(
            p.resolve_model_runtime("unknown-model", None, 4096, 3, 50)
                .is_err()
        );
    }

    /// US-001: once the catalog has answered, the declared window replaces the
    /// heuristic, and a slug the catalog does not describe stays unknown instead
    /// of borrowing the default.
    #[test]
    fn remote_catalog_window_overrides_embedded_descriptor() {
        let p = provider();
        assert_eq!(p.context_window_for_model("gpt-5.4"), Some(272_000));
        assert_eq!(p.max_context_for_model("gpt-5.4"), 272_000);
        p.catalog
            .write()
            .expect("catalog lock")
            .install_remote(
                include_str!("../fixtures/models-2026-07-28.json"),
                "2026-07-28",
            )
            .expect("fixture installs");
        assert_eq!(p.context_window_for_model("fixture-lite"), Some(200_000));
        assert_eq!(p.max_context_for_model("fixture-lite"), 200_000);
    }

    #[test]
    fn ultra_reasoning_effort_is_sent_as_max() {
        assert_eq!(reasoning_effort_for_request("ultra"), "max");
        assert_eq!(reasoning_effort_for_request("Ultra"), "max");
        assert_eq!(reasoning_effort_for_request("xhigh"), "xhigh");
    }

    #[test]
    fn replay_downgrade_only_matches_attributable_bad_requests() {
        assert!(should_retry_without_reasoning_replay(
            400,
            "encrypted_reasoning item is incompatible"
        ));
        assert!(should_retry_without_reasoning_replay(
            400,
            "reasoning replay is unsupported"
        ));
        assert!(!should_retry_without_reasoning_replay(400, "invalid image"));
        assert!(!should_retry_without_reasoning_replay(
            500,
            "encrypted_reasoning"
        ));
        let p = provider();
        assert_eq!(
            p.classify_error(&ProviderError::Http {
                status: 400,
                message: "encrypted_reasoning is not supported".into(),
                retry_after_ms: None,
            }),
            ErrorClass::ReasoningReplayRejected
        );
    }

    // US-029: session_id = well-formed UUID v4, stable per instance, unique.
    #[test]
    fn session_id_is_uuid_v4_shaped() {
        let id = new_session_id();
        assert_eq!(id.len(), 36, "UUID canonique 8-4-4-4-12");
        let lens: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(lens, vec![8, 4, 4, 4, 12]);
        assert_eq!(id.as_bytes()[14], b'4', "nibble de version 4");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant RFC 4122"
        );
        assert_ne!(new_session_id(), new_session_id(), "deux UUID diffèrent");
        // a provider carries a session_id UUID stored at construction time.
        assert_eq!(provider().prompt_cache_key().len(), 36);
    }

    #[test]
    fn prompt_cache_key_can_be_rebound_per_session() {
        let p = provider().with_prompt_cache_key("pyxis-session-a");
        assert_eq!(p.prompt_cache_key(), "pyxis-session-a");
        p.set_prompt_cache_key("pyxis-session-b");
        assert_eq!(p.prompt_cache_key(), "pyxis-session-b");
    }

    #[test]
    fn classify_error_taxonomy() {
        let p = provider();
        let http = |s| ProviderError::Http {
            status: s,
            message: String::new(),
            retry_after_ms: None,
        };
        assert!(matches!(
            p.classify_error(&http(401)),
            ErrorClass::Auth(AuthError::Expired)
        ));
        assert!(matches!(
            p.classify_error(&http(429)),
            ErrorClass::RateLimited
        ));
        assert!(matches!(
            p.classify_error(&http(529)),
            ErrorClass::Overloaded(529)
        ));
        assert!(matches!(
            p.classify_error(&http(503)),
            ErrorClass::Retryable
        ));
        assert!(matches!(
            p.classify_error(&http(400)),
            ErrorClass::InvalidRequest
        ));
        assert!(matches!(
            p.classify_error(&ProviderError::Transport("x".into())),
            ErrorClass::Retryable
        ));
    }

    // US-023: a 429 with an "exhausted quota" (GoUsageLimitError/billing body) is
    // TERMINAL (InvalidRequest, never retried); a transient 429 stays
    // RateLimited (retried).
    #[test]
    fn terminal_429_is_not_retried() {
        let p = provider();
        let terminal = |body: &str| ProviderError::Http {
            status: 429,
            message: body.to_string(),
            retry_after_ms: None,
        };
        for body in [
            "{\"error\":{\"type\":\"GoUsageLimitError\"}}",
            "FreeUsageLimitError: monthly usage limit reached",
            "{\"detail\":\"insufficient_quota\"}",
            "billing: out of budget",
        ] {
            assert!(
                matches!(
                    p.classify_error(&terminal(body)),
                    ErrorClass::InvalidRequest
                ),
                "terminal 429 expected for: {body}"
            );
        }
        // transient overload: no marker -> retryable.
        assert!(matches!(
            p.classify_error(&terminal("Too Many Requests, slow down")),
            ErrorClass::RateLimited
        ));
        // regression: a transient 429 mentioning "billing" must NOT be
        // classified terminal (bare substring ruled out, safe false-negative bias).
        assert!(matches!(
            p.classify_error(&terminal(
                "rate limited; see your billing dashboard for limits"
            )),
            ErrorClass::RateLimited
        ));
    }

    #[test]
    fn terminal_rate_limit_markers_are_case_insensitive() {
        assert!(is_terminal_rate_limit("GOUSAGELIMITERROR"));
        assert!(is_terminal_rate_limit("Quota Exceeded"));
        assert!(!is_terminal_rate_limit("transient overload, retry"));
    }

    #[test]
    fn every_401_is_classified_for_one_refresh_without_body_heuristics() {
        let p = provider();
        assert!(matches!(
            p.classify_error(&ProviderError::Http {
                status: 401,
                message: "access token expired".into(),
                retry_after_ms: None,
            }),
            ErrorClass::Auth(AuthError::Expired)
        ));
        assert!(matches!(
            p.classify_error(&ProviderError::Http {
                status: 401,
                message: "invalid token".into(),
                retry_after_ms: None,
            }),
            ErrorClass::Auth(AuthError::Expired)
        ));
        assert_eq!(
            p.classify_error(&ProviderError::Credential(AuthError::ReconnectRequired)),
            ErrorClass::Auth(AuthError::ReconnectRequired)
        );
    }

    #[test]
    fn error_body_sanitizer_redacts_tokens_and_account_ids() {
        let json = r#"{"error":"bad","access_token":"AT","refresh_token":"RT","nested":{"chatgpt_account_id":"acct_1"}}"#;
        let redacted = sanitize_error_body(json);
        assert!(!redacted.contains("AT"));
        assert!(!redacted.contains("RT"));
        assert!(!redacted.contains("acct_1"));
        assert!(redacted.contains("[redacted]"));

        let raw =
            "Authorization: Bearer AT_SECRET\nrefresh_token=RT_SECRET&chatgpt-account-id=acct_2";
        let redacted = sanitize_error_body(raw);
        assert!(!redacted.contains("AT_SECRET"));
        assert!(!redacted.contains("RT_SECRET"));
        assert!(!redacted.contains("acct_2"));
    }

    #[tokio::test]
    async fn disconnect_invalidates_in_memory_credential() {
        let p = provider();
        p.disconnect_auth().await.unwrap();
        let err = p.creds.request_spec().await.unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Credential(AuthError::ReconnectRequired)
        ));
    }

    #[tokio::test]
    async fn expiring_inference_credential_requests_one_visible_core_refresh() {
        let p = OpenAiChatGptProvider::new(
            OAuthCredential {
                provider: agent_auth::ProviderId::OpenAiChatGpt,
                access: agent_auth::Secret::new("AT"),
                refresh: agent_auth::Secret::new("RT"),
                expires_at: 0,
                account_id: Some("acct".into()),
            },
            DEFAULT_MAX_CONTEXT,
            None,
        );

        assert!(matches!(
            p.creds.request_spec().await,
            Err(ProviderError::Credential(AuthError::Expired))
        ));
    }

    // US-023: `Retry-After` parsing in the 3 formats (exact ms, whole
    // seconds, HTTP date), `retry-after-ms` taking priority.
    #[test]
    fn parse_retry_after_all_formats() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        // exact ms take priority (even when a `Retry-After` in seconds is present).
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("1500"));
        h.insert(RETRY_AFTER, HeaderValue::from_static("9"));
        assert_eq!(parse_retry_after_ms(&h, 0), Some(1500));

        // whole seconds -> ms.
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after_ms(&h, 0), Some(2000));

        // absolute HTTP date -> delta vs now. 30 s epoch, now 20 s -> 10 s left.
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:30 GMT"),
        );
        assert_eq!(parse_retry_after_ms(&h, 20_000), Some(10_000));

        // deadline already past -> 0 (saturating), never negative.
        assert_eq!(parse_retry_after_ms(&h, 40_000), Some(0));

        // no header -> None.
        assert_eq!(parse_retry_after_ms(&HeaderMap::new(), 0), None);
    }

    #[test]
    fn imf_fixdate_epoch_anchor() {
        // anchor: 1970-01-01T00:00:00Z = 0 ms.
        assert_eq!(
            parse_imf_fixdate_ms("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
        // one day later = 86_400_000 ms.
        assert_eq!(
            parse_imf_fixdate_ms("Fri, 02 Jan 1970 00:00:00 GMT"),
            Some(86_400_000)
        );
        // invalid format -> None (no panic).
        assert_eq!(parse_imf_fixdate_ms("pas une date"), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    // US-022: an OPEN but silent stream triggers the idle timeout (Retryable),
    // without freezing. Short (real) timeout -> fast and deterministic test.
    #[tokio::test]
    async fn idle_timeout_fires_on_silent_stream() {
        let silent = futures_util::stream::pending::<Result<StreamEvent, ProviderError>>().boxed();
        let guarded = idle_guarded(silent, Duration::from_millis(40));
        futures_util::pin_mut!(guarded);
        let first = guarded.next().await;
        assert!(
            matches!(&first, Some(Err(ProviderError::Stream(m))) if m == "idle timeout"),
            "idle timeout expected, got: {first:?}"
        );
    }

    // US-022 (hardening): a backend that ACCEPTS the connection then WITHHOLDS its
    // headers (blocked proxy, queue) must trigger the header timeout, not freeze the
    // loop. Local server that accepts then sleeps without answering; short (real) timeout.
    #[tokio::test]
    async fn header_timeout_fires_when_backend_withholds_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind localhost");
        let addr = listener.local_addr().expect("local addr");
        // accepts the socket and keeps it open WITHOUT ever writing a response.
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let rb = reqwest::Client::new().post(format!("http://{addr}/"));
        let res = send_with_header_timeout(rb, Duration::from_millis(150)).await;
        assert!(
            matches!(&res, Err(ProviderError::Stream(m)) if m == "header timeout"),
            "header timeout expected, got: {res:?}"
        );
    }

    // US-022: a stream emitting events under the delay relays them intact, and the
    // end of stream (None) terminates cleanly.
    #[tokio::test]
    async fn idle_guard_passes_events_through() {
        let inner = futures_util::stream::iter(vec![
            Ok(StreamEvent::TextDelta { text: "a".into() }),
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn,
            }),
        ])
        .boxed();
        let guarded = idle_guarded(inner, Duration::from_secs(5));
        let collected: Vec<_> = guarded.collect().await;
        assert_eq!(collected.len(), 2);
        assert!(matches!(collected[0], Ok(StreamEvent::TextDelta { .. })));
        assert!(matches!(
            collected[1],
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn
            })
        ));
    }

    // US-022: an upstream error is relayed then ends the stream (parity with the
    // direct path: `yield Err` then `return`).
    #[tokio::test]
    async fn idle_guard_propagates_and_stops_on_error() {
        let inner = futures_util::stream::iter(vec![
            Ok(StreamEvent::TextDelta { text: "x".into() }),
            Err(ProviderError::Stream("boom".into())),
            Ok(StreamEvent::TextDelta {
                text: "never".into(),
            }),
        ])
        .boxed();
        let guarded = idle_guarded(inner, Duration::from_secs(5));
        let collected: Vec<_> = guarded.collect().await;
        assert_eq!(collected.len(), 2, "should stop after the error");
        assert!(matches!(collected[1], Err(ProviderError::Stream(_))));
    }
}
