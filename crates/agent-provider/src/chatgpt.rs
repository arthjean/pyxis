//! `OpenAiChatGpt` adapter: ChatGPT subscription through the Responses API on the
//! ChatGPT/Codex backend (ADR-10). Implements `agent_core::Provider`.
//!
//! The canonical transcript remains client-side. WebSocket may reuse
//! `previous_response_id` only for a strict extension inside one turn; every new
//! turn and every HTTP fallback starts from the complete canonical transcript.
//!
//! The backend can receive `include: ["reasoning.encrypted_content"]` when
//! reasoning replay is explicitly enabled. By default, that path stays OFF
//! until the post-rename wire format is validated live.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_auth::OAuthCredential;
use agent_core::model::{ModelRetryPolicy, ModelRuntimeError, ResolvedModelRuntime};
use agent_core::provider::{
    AuthError, CacheCapabilities, CanonicalRequest, Capabilities, CapabilityLimits, ErrorClass,
    Provider, ProviderError, ProviderKind, ReasoningCapabilities, StreamEvent,
    ToolCallingCapabilities,
};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use sha2::Digest;
use tokio_util::sync::CancellationToken;

use crate::chatgpt_error::invalid_request;
use crate::chatgpt_http::ResponsesTransportConfig;
use crate::chatgpt_websocket::{
    ResponsesWebSocket, WebSocketProbeAuthorization, WebSocketProbeExecution, WebSocketProbeReport,
};
use crate::credential::CredentialManager;
use crate::models::{CatalogModel, ModelCatalog};
use crate::responses;
use crate::responses::catalog::{CatalogCacheMode, CatalogRequest, RemoteCatalogManager};

/// Keyring key of the ChatGPT subscription credential (rotating refresh rewritten here).
pub const KEYRING_ACCOUNT: &str = "oauth:openai_chatgpt";

/// Default context window (GPT-5.x models of the Codex backend). **Volatile
/// value, to be adjusted**: it ONLY affects the compaction thresholds; a real
/// overflow triggers reactive compaction (413, withholding). Conservative.
pub const DEFAULT_MAX_CONTEXT: u32 = 256_000;

/// Default reasoning effort (Codex CLI is roughly "medium").
pub const DEFAULT_REASONING_EFFORT: &str = "medium";

const DEFAULT_CHATGPT_RETRY: ModelRetryPolicy = ModelRetryPolicy {
    max_attempts: 4,
    backoff_base_ms: 50,
};

/// Default model slug. The Codex backend (ChatGPT subscription) enforces an
/// allow-list of VERSIONED slugs that it keeps changing (frequent removals): the
/// generic `gpt-5` slug is rejected with a 400 ("not supported when using Codex with
/// a ChatGPT account"). **Volatile value**, overridable through `--model` or the
/// `/models` command in session (see `agent_tui::MODELS`).
pub const DEFAULT_MODEL: &str = "gpt-5.5";

/// Default per-event idle timeout (US-022). An OPEN SSE stream that emits
/// no more events (silent backend, queue) is cancelled after this delay ->
/// `Stream("idle timeout")` (Retryable). Configurable per session (`with_idle_timeout`,
/// env `PYXIS_IDLE_TIMEOUT_SECS`). Pi: 20 s (header); Codex CLI: 300 s/event.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct OpenAiChatGptProvider {
    creds: Arc<CredentialManager>,
    http: reqwest::Client,
    capabilities: Capabilities,
    reasoning_effort: Option<String>,
    config: ResponsesTransportConfig,
    /// STABLE session identifier (UUID v4), sent as `prompt_cache_key` on
    /// every request (US-029) -> the backend reuses its prefix cache.
    session_id: RwLock<String>,
    /// Last valid remote catalog layered over the versioned embedded fallback.
    /// Shared by interactive discovery and headless turn resolution.
    catalog: Arc<RwLock<ModelCatalog>>,
    catalog_manager: RemoteCatalogManager,
    websocket: ResponsesWebSocket,
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
    pub fn new(
        cred: OAuthCredential,
        max_context: u32,
        reasoning_effort: Option<String>,
    ) -> Result<Self, ProviderError> {
        Self::from_validated_config(
            cred,
            max_context,
            reasoning_effort,
            ResponsesTransportConfig::chatgpt_default(),
        )
    }

    /// Builds an adapter with an explicit provider policy. The endpoint origin
    /// is rejected before a credential manager can expose OAuth headers.
    pub fn new_with_config(
        cred: OAuthCredential,
        max_context: u32,
        reasoning_effort: Option<String>,
        config: ResponsesTransportConfig,
    ) -> Result<Self, ProviderError> {
        config.validate_for_chatgpt()?;
        Self::from_validated_config(cred, max_context, reasoning_effort, config)
    }

    fn from_validated_config(
        cred: OAuthCredential,
        max_context: u32,
        reasoning_effort: Option<String>,
        config: ResponsesTransportConfig,
    ) -> Result<Self, ProviderError> {
        // US-022: `connect_timeout` bounds the TCP/TLS establishment. A `build()`
        // failure (TLS backend unavailable) falls back on the default client:
        // never a panic (`panic = deny` lint).
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let creds = Arc::new(CredentialManager::new(cred, http.clone(), KEYRING_ACCOUNT));
        let catalog = Arc::new(RwLock::new(ModelCatalog::embedded()));
        let catalog_manager = RemoteCatalogManager::new(
            http.clone(),
            Arc::clone(&catalog),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let capabilities = Capabilities {
            vision: true,
            tools: true,
            structured_output: true,
            // The adapter sends an explicit prompt cache key for every session.
            prompt_caching: true,
            reasoning: true,
            // WebSocket continuation is transport-local and does not move
            // ownership of the canonical transcript to the server.
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
                namespace_tools: true,
                tool_search: true,
                web_search: true,
            },
            reasoning_options: ReasoningCapabilities {
                encrypted_replay: true,
            },
            cache: CacheCapabilities {
                prompt_cache_key: true,
            },
        };
        capabilities.validate()?;
        Ok(Self {
            creds,
            http,
            capabilities,
            reasoning_effort,
            config,
            session_id: RwLock::new(new_session_id()),
            catalog,
            catalog_manager,
            websocket: ResponsesWebSocket::new(),
        })
    }

    /// Convenience constructor: MVP defaults (`DEFAULT_MAX_CONTEXT`, medium effort).
    pub fn from_credential(cred: OAuthCredential) -> Result<Self, ProviderError> {
        Self::new(
            cred,
            DEFAULT_MAX_CONTEXT,
            Some(DEFAULT_REASONING_EFFORT.to_string()),
        )
    }

    /// Declares that this process can run Code Mode cells. Without it, a model
    /// whose tool mode needs code mode stays visible and is refused before any
    /// request, naming the missing runtime (PRD edge case 1).
    pub fn set_code_mode(&self, available: bool) {
        if let Ok(mut catalog) = self.catalog.write() {
            catalog.set_code_mode(available);
        }
    }

    /// Overrides the SSE idle timeout (US-022). `Duration::ZERO` is ignored (keeps
    /// the default) so that an absurd env value does not disable the watchdog.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.config.set_idle_timeout(idle);
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

    fn runtime_for_request(
        &self,
        request: &CanonicalRequest,
    ) -> Result<ResolvedModelRuntime, ProviderError> {
        if let Some(runtime) = &request.model_runtime {
            return Ok(runtime.clone());
        }
        self.resolve_model_runtime(
            &request.model,
            request.reasoning_effort.as_deref(),
            request.max_output_tokens,
            DEFAULT_CHATGPT_RETRY.max_attempts.saturating_sub(1),
            DEFAULT_CHATGPT_RETRY.backoff_base_ms,
        )
        .map_err(invalid_request)
    }

    fn prepare_request(
        &self,
        req: CanonicalRequest,
    ) -> Result<responses::ResponsesPlan, ProviderError> {
        let cache_key = self.prompt_cache_key();
        responses::prepare(
            &self.config,
            &self.capabilities,
            req,
            |request| self.runtime_for_request(request),
            false,
            Some(&cache_key),
        )
    }

    /// Establishes the persistent transport without sending model input. It is
    /// safe to call repeatedly and is a no-op after session fallback to SSE.
    pub async fn preconnect_websocket(&self, req: CanonicalRequest) -> Result<(), ProviderError> {
        let plan = self.prepare_request(req)?;
        if !self.config.websocket_enabled() {
            return Ok(());
        }
        let auth = self.creds.request_spec().await?;
        self.websocket
            .preconnect(&auth, &self.config, plan.request(), plan.route())
            .await
    }

    /// Runs the opt-in live handshake, `generate=false`, continuation, terminal,
    /// and close proof without mutating the provider session state.
    pub async fn probe_websocket(
        &self,
        authorization: WebSocketProbeAuthorization,
        req: CanonicalRequest,
    ) -> Result<WebSocketProbeReport, ProviderError> {
        let plan = self.prepare_request(req)?;
        let auth = self.creds.request_spec().await?;
        Ok(self
            .websocket
            .probe(WebSocketProbeExecution {
                authorization,
                auth: &auth,
                config: &self.config,
                request: plan.request(),
                route: plan.route(),
                full_body: plan.body().clone(),
                body_bytes: plan.body_bytes(),
            })
            .await)
    }

    /// Catalog of the models offered to the connected account (`GET /models`), sorted by
    /// backend priority. Discovered at runtime: never a list frozen in the
    /// binary (the backend removes/adds slugs without notice). Outside the
    /// `Provider` trait: the notion of a catalog is specific to this adapter.
    pub async fn list_models(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        self.catalog_manager
            .refresh(
                chatgpt_catalog_request(
                    Arc::clone(&self.creds),
                    self.catalog_manager.scope_epoch(),
                )
                .await?,
                CatalogCacheMode::Revalidate,
                None,
            )
            .await
    }

    /// Forces a network fetch without sending the cached ETag. The resulting
    /// snapshot is still installed atomically and scoped like the normal path.
    pub async fn list_models_uncached(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        self.catalog_manager
            .refresh(
                chatgpt_catalog_request(
                    Arc::clone(&self.creds),
                    self.catalog_manager.scope_epoch(),
                )
                .await?,
                CatalogCacheMode::Bypass,
                None,
            )
            .await
    }

    fn observe_catalog_etag(
        &self,
        stream: BoxStream<'static, Result<StreamEvent, ProviderError>>,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let creds = Arc::clone(&self.creds);
        let catalog_manager = self.catalog_manager.clone();
        self.catalog_manager.observe(stream, move || {
            let creds = Arc::clone(&creds);
            let epoch = catalog_manager.scope_epoch();
            async move { chatgpt_catalog_request(creds, epoch).await }
        })
    }

    fn catalog_window(&self, model: &str) -> Option<u32> {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.context_window(model.trim()))
    }
}

async fn chatgpt_catalog_request(
    creds: Arc<CredentialManager>,
    scope_epoch: u64,
) -> Result<CatalogRequest, ProviderError> {
    let auth = creds.models_spec().await?;
    let account_id = auth
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
        .map(|(_, value)| value.as_str())
        .ok_or(ProviderError::Credential(
            agent_core::provider::AuthError::ReconnectRequired,
        ))?;
    let identity_fingerprint = hex::encode(sha2::Sha256::digest(account_id.as_bytes()));
    CatalogRequest::new("openai_chatgpt", auth, identity_fingerprint, scope_epoch)
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
        let retry = self.config.effective_retry(ModelRetryPolicy {
            max_attempts: max_retries.saturating_add(1),
            backoff_base_ms,
        });
        catalog.resolve(
            model,
            reasoning_effort.or(self.reasoning_effort.as_deref()),
            max_output_tokens,
            retry,
        )
    }

    fn tool_mode(&self, slug: &str) -> agent_core::model::ModelToolMode {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.tool_mode(slug))
            .unwrap_or(agent_core::model::ModelToolMode::Direct)
    }

    fn multi_agent_version(&self, slug: &str) -> agent_core::model::MultiAgentVersion {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.multi_agent_version(slug))
            .unwrap_or_default()
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let plan = self.prepare_request(req)?;
        let auth = self.creds.request_spec().await?;
        let stream = responses::stream(
            responses::ResponsesExecution {
                http: &self.http,
                websocket: &self.websocket,
                config: &self.config,
                auth: &auth,
            },
            plan,
            CancellationToken::new(),
        )
        .await?;
        Ok(self.observe_catalog_etag(stream))
    }

    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        if let Err(error) = self.creds.force_refresh().await {
            if matches!(
                error,
                ProviderError::Credential(AuthError::RecoveryPermanent)
            ) {
                self.websocket.disconnect(&self.config).await;
                if let Err(cleanup) = self.catalog_manager.invalidate_scope() {
                    tracing::warn!(
                        target: "pyxis::models",
                        error = %cleanup,
                        "catalog cleanup failed after permanent credential recovery failure"
                    );
                }
            }
            return Err(error);
        }
        self.websocket.reset_scope();
        self.catalog_manager.invalidate_scope()?;
        Ok(())
    }

    async fn disconnect_auth(&self) -> Result<(), ProviderError> {
        self.websocket.disconnect(&self.config).await;
        self.creds.disconnect().await;
        self.catalog_manager.invalidate_scope()?;
        Ok(())
    }

    fn set_prompt_cache_key(&self, key: &str) {
        let mut changed = false;
        if let Ok(mut session_id) = self.session_id.write()
            && session_id.as_str() != key
        {
            *session_id = key.to_string();
            changed = true;
        }
        if changed {
            self.websocket.reset_scope();
        }
    }

    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        responses::classify_error(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::{AuthError, ProviderErrorCategory, StopReason, ToolSpec};
    use futures_util::StreamExt;

    use crate::chatgpt_error::{
        api_category_for_http, days_from_civil, is_terminal_rate_limit, parse_imf_fixdate_ms,
        parse_retry_after_ms, sanitize_error_body, should_retry_without_reasoning_replay,
    };

    fn credential(expires_at: u64) -> OAuthCredential {
        OAuthCredential {
            provider: agent_auth::ProviderId::OpenAiChatGpt,
            access: agent_auth::Secret::new("AT"),
            refresh: agent_auth::Secret::new("RT"),
            expires_at,
            account_id: Some("acct".into()),
        }
    }

    fn provider() -> OpenAiChatGptProvider {
        OpenAiChatGptProvider::new(credential(u64::MAX), DEFAULT_MAX_CONTEXT, None).unwrap()
    }

    #[test]
    fn capabilities_preserve_client_side_state_with_websocket_transport() {
        let p = provider();
        let c = p.capabilities();
        assert!(!c.server_side_state);
        assert!(c.tools && c.reasoning);
        assert!(c.reasoning_options.encrypted_replay);
        assert!(c.prompt_caching && c.cache.prompt_cache_key);
        c.validate().unwrap();
        assert_eq!(p.kind(), ProviderKind::OpenAiChatGpt);
    }

    #[test]
    fn constructor_rejects_invalid_capabilities() {
        assert!(matches!(
            OpenAiChatGptProvider::new(credential(u64::MAX), 0, None),
            Err(ProviderError::UnsupportedCapability { .. })
        ));
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
            ..CanonicalRequest::default()
        };
        assert!(
            matches!(
                p.stream(request).await,
                Err(ProviderError::Api {
                    category: ProviderErrorCategory::InvalidRequest,
                    status: None,
                    ref message,
                    ..
                }) if message.contains("model is empty")
            ),
            "invalid request must fail before opening a stream"
        );
    }

    #[tokio::test]
    async fn transport_headers_are_validated_before_expired_credentials() {
        let provider =
            OpenAiChatGptProvider::new(credential(0), DEFAULT_MAX_CONTEXT, None).unwrap();
        let request = CanonicalRequest {
            model: "gpt-5.5".into(),
            client_metadata: std::collections::BTreeMap::from([(
                "thread_id".into(),
                "invalid\nheader".into(),
            )]),
            max_output_tokens: 4096,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            provider.stream(request).await,
            Err(ProviderError::Api {
                category: ProviderErrorCategory::InvalidRequest,
                ref message,
                ..
            }) if message.contains("client metadata")
        ));
    }

    #[test]
    fn selected_model_rejects_unsupported_tools_before_transport_preparation() {
        let provider = provider();
        let request = CanonicalRequest {
            model: "gpt-5.5".into(),
            tools: vec![ToolSpec::tool_search(
                "find tools",
                "client",
                serde_json::json!({"type":"object","properties":{}}),
            )],
            max_output_tokens: 4096,
            ..CanonicalRequest::default()
        };
        assert!(matches!(
            provider.prepare_request(request),
            Err(ProviderError::UnsupportedTool { tool, reason })
                if tool == "tool_search" && reason.contains("selected model")
        ));
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

    #[test]
    fn explicit_provider_retry_is_applied_during_runtime_resolution() {
        let config = ResponsesTransportConfig::chatgpt_default()
            .with_retry(ModelRetryPolicy {
                max_attempts: 5,
                backoff_base_ms: 75,
            })
            .unwrap();
        let provider = OpenAiChatGptProvider::new_with_config(
            credential(u64::MAX),
            DEFAULT_MAX_CONTEXT,
            None,
            config,
        )
        .unwrap();
        let runtime = provider
            .resolve_model_runtime("gpt-5.5", None, 4096, 1, 10)
            .unwrap();
        assert_eq!(
            runtime.retry,
            ModelRetryPolicy {
                max_attempts: 5,
                backoff_base_ms: 75,
            }
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

    #[tokio::test]
    async fn catalog_scope_hashes_identity_and_ignores_request_query() {
        let manager = Arc::new(CredentialManager::new(
            OAuthCredential {
                account_id: Some("account-secret-id".into()),
                ..credential(u64::MAX)
            },
            reqwest::Client::new(),
            KEYRING_ACCOUNT,
        ));
        let request = chatgpt_catalog_request(manager, 0)
            .await
            .expect("scope builds");
        let scope = request.scope();
        assert_eq!(scope.provider, "openai_chatgpt");
        assert!(!scope.endpoint.contains("client_version"));
        assert!(!scope.identity_fingerprint.contains("account-secret-id"));

        let other = Arc::new(CredentialManager::new(
            OAuthCredential {
                account_id: Some("other-account".into()),
                ..credential(u64::MAX)
            },
            reqwest::Client::new(),
            KEYRING_ACCOUNT,
        ));
        let other = chatgpt_catalog_request(other, 0)
            .await
            .expect("other scope");
        assert_ne!(
            scope.identity_fingerprint,
            other.scope().identity_fingerprint
        );
    }

    #[test]
    fn ultra_reasoning_effort_is_sent_as_max() {
        assert_eq!(responses::reasoning_effort_for_request("ultra"), "max");
        assert_eq!(responses::reasoning_effort_for_request("Ultra"), "max");
        assert_eq!(responses::reasoning_effort_for_request("xhigh"), "xhigh");
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
        let api = |category, status| ProviderError::Api {
            category,
            status,
            message: String::new(),
            retry_after_ms: None,
            request_id: None,
            auth_request_id: None,
        };
        assert_eq!(
            p.classify_error(&api(ProviderErrorCategory::Incomplete, Some(400))),
            ErrorClass::Retryable
        );
        assert_eq!(
            p.classify_error(&api(ProviderErrorCategory::Failed, Some(503))),
            ErrorClass::Retryable
        );
        assert_eq!(
            p.classify_error(&api(ProviderErrorCategory::Failed, None)),
            ErrorClass::InvalidRequest,
            "an unknown post-dispatch outcome must never be replayed"
        );
        assert_eq!(
            p.classify_error(&api(ProviderErrorCategory::Quota, Some(429))),
            ErrorClass::InvalidRequest
        );
    }

    #[test]
    fn http_status_and_body_codes_map_to_typed_categories() {
        assert_eq!(
            api_category_for_http(400, r#"{"error":{"code":"invalid_image"}}"#),
            ProviderErrorCategory::InvalidImage
        );
        assert_eq!(
            api_category_for_http(429, r#"{"error":{"code":"insufficient_quota"}}"#),
            ProviderErrorCategory::Quota
        );
        assert_eq!(
            api_category_for_http(401, "unauthorized"),
            ProviderErrorCategory::Authentication
        );
        assert_eq!(
            api_category_for_http(403, "forbidden"),
            ProviderErrorCategory::PermissionDenied
        );
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
        assert!(redacted.contains("[REDACTED]"));

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
        )
        .unwrap();

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
        let guarded = responses::idle_guarded(silent, Duration::from_millis(40));
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
        let client = reqwest::Client::new();
        let request = client
            .post(format!("http://{addr}/"))
            .build()
            .expect("local request");
        let res =
            responses::send_with_header_timeout(&client, request, Duration::from_millis(150)).await;
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
        let guarded = responses::idle_guarded(inner, Duration::from_secs(5));
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
        let guarded = responses::idle_guarded(inner, Duration::from_secs(5));
        let collected: Vec<_> = guarded.collect().await;
        assert_eq!(collected.len(), 2, "should stop after the error");
        assert!(matches!(collected[1], Err(ProviderError::Stream(_))));
    }
}
