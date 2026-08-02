//! Configured OpenAI-compatible Responses provider.

use std::sync::{Arc, Mutex as StdMutex, RwLock};

use agent_auth::provider::{ProviderAuthTarget, ProviderCredential};
use agent_core::model::{ModelRetryPolicy, ModelRuntimeError, ResolvedModelRuntime};
use agent_core::provider::{
    AuthError, CanonicalRequest, Capabilities, ErrorClass, Provider, ProviderError, ProviderKind,
    StreamEvent,
};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::chatgpt_error::invalid_request;
use crate::chatgpt_websocket::ResponsesWebSocket;
use crate::models::ModelCatalog;
use crate::responses;
use crate::responses::catalog::RemoteCatalogManager;

mod auth;
mod catalog;
mod config;

use auth::auth_config_error;
pub use auth::{AuthRecovery, OpenAiAccountState};
pub use config::{
    ConfiguredOpenAiConfig, OpenAiAuthPolicy, OpenAiCatalogPolicy, OpenAiEndpointKind,
};

const DEFAULT_RETRY: ModelRetryPolicy = ModelRetryPolicy {
    max_attempts: 4,
    backoff_base_ms: 50,
};

pub struct ConfiguredOpenAiProvider {
    config: ConfiguredOpenAiConfig,
    http: reqwest::Client,
    credential: Arc<tokio::sync::RwLock<Option<ProviderCredential>>>,
    recovery: Option<Arc<dyn AuthRecovery>>,
    capabilities: Capabilities,
    catalog: Arc<RwLock<ModelCatalog>>,
    catalog_manager: RemoteCatalogManager,
    websocket: ResponsesWebSocket,
    scope_cancel: StdMutex<CancellationToken>,
    prompt_cache_key: RwLock<Option<String>>,
}

impl ConfiguredOpenAiProvider {
    pub fn new(
        config: ConfiguredOpenAiConfig,
        credential: ProviderCredential,
        recovery: Option<Arc<dyn AuthRecovery>>,
    ) -> Result<Self, ProviderError> {
        config.validate()?;
        let endpoint = config.endpoint()?;
        credential
            .resolve(&ProviderAuthTarget::OpenAi {
                provider: config.auth_provider,
                endpoint,
                allow_unauthenticated: config.allow_unauthenticated(),
            })
            .map_err(auth_config_error)?;
        let catalog = match &config.catalog {
            OpenAiCatalogPolicy::Static(models) => ModelCatalog::from_static(models.clone()),
            OpenAiCatalogPolicy::Remote { .. } => Ok(ModelCatalog::remote_only()),
        }
        .map_err(|error| invalid_request(format!("invalid provider field `models`: {error}")))?;
        let http = reqwest::Client::builder()
            .connect_timeout(config.transport.connect_timeout())
            .build()
            .map_err(|_| invalid_request("invalid provider field `http_client`"))?;
        let catalog = Arc::new(RwLock::new(catalog));
        let catalog_manager = RemoteCatalogManager::new(
            http.clone(),
            Arc::clone(&catalog),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        Ok(Self {
            capabilities: config.capabilities.clone(),
            config,
            http,
            credential: Arc::new(tokio::sync::RwLock::new(Some(credential))),
            recovery,
            catalog,
            catalog_manager,
            websocket: ResponsesWebSocket::new(),
            scope_cancel: StdMutex::new(CancellationToken::new()),
            prompt_cache_key: RwLock::new(None),
        })
    }

    pub fn set_code_mode(&self, available: bool) {
        if let Ok(mut catalog) = self.catalog.write() {
            catalog.set_code_mode(available);
        }
    }

    fn prepare_request(
        &self,
        request: CanonicalRequest,
    ) -> Result<responses::ResponsesPlan, ProviderError> {
        let prompt_cache_key = self
            .capabilities
            .cache
            .prompt_cache_key
            .then(|| {
                self.prompt_cache_key
                    .read()
                    .ok()
                    .and_then(|key| key.clone())
            })
            .flatten();
        responses::prepare(
            &self.config.transport,
            &self.capabilities,
            request,
            |request| {
                if let Some(runtime) = &request.model_runtime {
                    Ok(runtime.clone())
                } else {
                    self.resolve_model_runtime(
                        &request.model,
                        request.reasoning_effort.as_deref(),
                        request.max_output_tokens,
                        DEFAULT_RETRY.max_attempts.saturating_sub(1),
                        DEFAULT_RETRY.backoff_base_ms,
                    )
                    .map_err(invalid_request)
                }
            },
            self.config.endpoint_kind.stores_responses(),
            prompt_cache_key.as_deref(),
        )
    }

    fn cancellation_snapshot(&self) -> CancellationToken {
        self.scope_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .child_token()
    }
}

#[async_trait]
impl Provider for ConfiguredOpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiResponses
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
            reasoning_effort,
            max_output_tokens,
            self.config.transport.effective_retry(ModelRetryPolicy {
                max_attempts: max_retries.saturating_add(1),
                backoff_base_ms,
            }),
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
        request: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let plan = self.prepare_request(request)?;
        let endpoint = self.config.endpoint()?;
        let (_, auth) = self.resolved_auth().await?;
        let request_auth = Self::request_auth(&endpoint, &auth);
        let stream = responses::stream(
            responses::ResponsesExecution {
                http: &self.http,
                websocket: &self.websocket,
                config: &self.config.transport,
                auth: &request_auth,
            },
            plan,
            self.cancellation_snapshot(),
        )
        .await?;
        Ok(self.observe_catalog_etag(stream))
    }

    fn classify_error(&self, error: &ProviderError) -> ErrorClass {
        responses::classify_error(error)
    }

    async fn refresh_auth(&self) -> Result<(), ProviderError> {
        let recovery = self
            .recovery
            .as_ref()
            .ok_or(ProviderError::Credential(AuthError::RecoveryUnavailable))?;
        let account = self.account_state().await?;
        let credential = match recovery.recover(&account).await {
            Ok(credential) => credential,
            Err(error) => {
                if error == AuthError::RecoveryPermanent {
                    *self.credential.write().await = None;
                    self.invalidate_scope();
                    self.websocket.disconnect(&self.config.transport).await;
                }
                return Err(ProviderError::Credential(error));
            }
        };
        self.replace_credential(credential).await?;
        Ok(())
    }

    async fn disconnect_auth(&self) -> Result<(), ProviderError> {
        *self.credential.write().await = None;
        self.invalidate_scope();
        self.websocket.disconnect(&self.config.transport).await;
        Ok(())
    }

    fn set_prompt_cache_key(&self, key: &str) {
        if let Ok(mut current) = self.prompt_cache_key.write() {
            *current = Some(key.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_auth::{ProviderId, Secret};
    use agent_core::message::Message;
    use agent_core::model::{
        InputModality, ModelDescriptor, ModelToolMode, MultiAgentVersion, ReasoningReplaySupport,
        ResponsesDialect, TruncationMode, TruncationPolicy,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::catalog::configured_catalog_request;
    use crate::chatgpt_http::ResponsesTransportConfig;
    use crate::models::CatalogScope;

    fn descriptor() -> ModelDescriptor {
        ModelDescriptor {
            slug: "test-model".into(),
            display_name: "Test".into(),
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

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            model: "test-model".into(),
            messages: vec![Message::user("hello")],
            max_output_tokens: 100,
            ..CanonicalRequest::default()
        }
    }

    fn provider(base: &str, endpoint_kind: OpenAiEndpointKind) -> ConfiguredOpenAiProvider {
        let transport = ResponsesTransportConfig::new(base, "responses")
            .unwrap()
            .with_websocket(false);
        let config = ConfiguredOpenAiConfig::new(
            "fixture",
            endpoint_kind,
            transport,
            OpenAiCatalogPolicy::Static(vec![descriptor()]),
        )
        .unwrap();
        ConfiguredOpenAiProvider::new(
            config,
            ProviderCredential::ApiKey {
                provider: ProviderId::OpenAiResponses,
                key: Secret::new("secret"),
                identity: None,
            },
            None,
        )
        .unwrap()
    }

    fn credential(secret: &str, identity: &str) -> ProviderCredential {
        ProviderCredential::ApiKey {
            provider: ProviderId::OpenAiResponses,
            key: Secret::new(secret),
            identity: Some(identity.into()),
        }
    }

    fn config() -> ConfiguredOpenAiConfig {
        ConfiguredOpenAiConfig::new(
            "fixture",
            OpenAiEndpointKind::Standard,
            ResponsesTransportConfig::new("https://example.test/v1/", "responses")
                .unwrap()
                .with_websocket(false),
            OpenAiCatalogPolicy::Static(vec![descriptor()]),
        )
        .unwrap()
    }

    struct FixedRecovery {
        calls: AtomicUsize,
        result: Result<ProviderCredential, AuthError>,
    }

    #[async_trait]
    impl AuthRecovery for FixedRecovery {
        async fn recover(
            &self,
            _account: &OpenAiAccountState,
        ) -> Result<ProviderCredential, AuthError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[test]
    fn azure_store_is_isolated_from_standard_responses() {
        let standard = provider("https://api.openai.com/v1/", OpenAiEndpointKind::Standard);
        let azure = provider(
            "https://resource.openai.azure.com/openai/",
            OpenAiEndpointKind::AzureResponses,
        );
        assert_eq!(
            standard.prepare_request(request()).unwrap().body()["store"],
            false
        );
        assert_eq!(
            azure.prepare_request(request()).unwrap().body()["store"],
            true
        );
    }

    #[tokio::test]
    async fn static_catalog_does_not_fetch() {
        let provider = provider(
            "https://unreachable.invalid/v1/",
            OpenAiEndpointKind::Standard,
        );
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "test-model");
    }

    #[tokio::test]
    async fn configured_remote_catalog_uses_the_current_client_version() {
        let config = ConfiguredOpenAiConfig::new(
            "fixture",
            OpenAiEndpointKind::Standard,
            ResponsesTransportConfig::new("https://example.test/v1/", "responses")
                .unwrap()
                .with_websocket(false),
            OpenAiCatalogPolicy::Remote {
                models_path: "models".into(),
            },
        )
        .unwrap();
        let request = configured_catalog_request(
            config,
            Arc::new(tokio::sync::RwLock::new(Some(credential(
                "secret", "account",
            )))),
            "models".into(),
            0,
        )
        .await
        .unwrap();
        let url = url::Url::parse(request.url()).unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(name, _)| name == "client_version")
                .map(|(_, value)| value.into_owned()),
            Some(agent_auth::oauth::openai_chatgpt::codex_client_version())
        );
    }

    #[tokio::test]
    async fn configured_headers_apply_to_the_remote_catalog_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let body = include_str!("../fixtures/models-2026-07-28.json");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let transport =
            ResponsesTransportConfig::new(&format!("http://{address}/v1/"), "responses")
                .unwrap()
                .with_default_header("x-tenant", "fixture-tenant")
                .unwrap()
                .with_websocket(false);
        let config = ConfiguredOpenAiConfig::new(
            "fixture",
            OpenAiEndpointKind::Standard,
            transport,
            OpenAiCatalogPolicy::Remote {
                models_path: "models".into(),
            },
        )
        .unwrap();
        let provider = ConfiguredOpenAiProvider::new(
            config,
            credential("catalog-secret", "fixture-account"),
            None,
        )
        .unwrap();
        assert!(!provider.list_models().await.unwrap().is_empty());
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models?client_version="));
        assert!(request.contains("x-tenant: fixture-tenant"));
        assert!(request.contains("authorization: bearer catalog-secret"));
    }

    #[test]
    fn incoherent_auth_fails_locally() {
        let transport = ResponsesTransportConfig::new("https://example.test/v1/", "responses")
            .unwrap()
            .with_websocket(false);
        let config = ConfiguredOpenAiConfig::new(
            "fixture",
            OpenAiEndpointKind::Standard,
            transport,
            OpenAiCatalogPolicy::Static(vec![descriptor()]),
        )
        .unwrap();
        let error = ConfiguredOpenAiProvider::new(
            config,
            ProviderCredential::BedrockApiKey {
                token: Secret::new("secret"),
                region: "eu-west-3".into(),
                identity: None,
            },
            None,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("auth"));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn missing_structured_output_capability_fails_before_network() {
        let mut provider = provider(
            "https://unreachable.invalid/v1/",
            OpenAiEndpointKind::Standard,
        );
        provider.capabilities.structured_output = false;
        let mut request = request();
        request.output_schema = Some(agent_core::provider::OutputSchema {
            name: "answer".into(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });
        assert!(matches!(
            provider.prepare_request(request),
            Err(ProviderError::UnsupportedCapability { .. })
        ));
    }

    #[test]
    fn config_debug_contains_no_header_values() {
        let transport = ResponsesTransportConfig::new("https://example.test/v1/", "responses")
            .unwrap()
            .with_default_header("x-secret", "do-not-log")
            .unwrap()
            .with_websocket(false);
        let config = ConfiguredOpenAiConfig::new(
            "fixture",
            OpenAiEndpointKind::Standard,
            transport,
            OpenAiCatalogPolicy::Static(vec![descriptor()]),
        )
        .unwrap();
        assert!(!format!("{config:?}").contains("do-not-log"));
    }

    #[tokio::test]
    async fn recovery_is_single_scoped_and_account_switch_invalidates_remote_state() {
        let recovery = Arc::new(FixedRecovery {
            calls: AtomicUsize::new(0),
            result: Ok(credential("new-secret", "new-account")),
        });
        let provider = ConfiguredOpenAiProvider::new(
            config(),
            credential("old-secret", "old-account"),
            Some(recovery.clone()),
        )
        .unwrap();
        let old = provider.account_state().await.unwrap();
        provider
            .catalog
            .write()
            .unwrap()
            .install_remote_scoped(
                include_str!("../fixtures/models-2026-07-28.json"),
                "fixture",
                CatalogScope {
                    provider: "fixture".into(),
                    endpoint: "https://example.test/v1/models".into(),
                    identity_fingerprint: old.identity_fingerprint.clone(),
                },
                Some("old-etag".into()),
            )
            .unwrap();

        provider.refresh_auth().await.unwrap();
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
        let new = provider.account_state().await.unwrap();
        assert_ne!(old.identity_fingerprint, new.identity_fingerprint);
        assert!(provider.catalog.read().unwrap().scope().is_none());
        let debug = format!("{old:?} {new:?}");
        assert!(!debug.contains("old-account"));
        assert!(!debug.contains("new-account"));
        assert!(!debug.contains("secret"));
    }

    #[tokio::test]
    async fn recovery_failures_remain_distinct_and_disconnect_revokes_state() {
        for (failure, expected) in [
            (AuthError::RecoveryPermanent, AuthError::RecoveryPermanent),
            (AuthError::RecoveryTransient, AuthError::RecoveryTransient),
            (
                AuthError::RecoveryUnavailable,
                AuthError::RecoveryUnavailable,
            ),
        ] {
            let provider = ConfiguredOpenAiProvider::new(
                config(),
                credential("secret", "account"),
                Some(Arc::new(FixedRecovery {
                    calls: AtomicUsize::new(0),
                    result: Err(failure),
                })),
            )
            .unwrap();
            assert!(matches!(
                provider.refresh_auth().await,
                Err(ProviderError::Credential(error)) if error == expected
            ));
            if failure == AuthError::RecoveryPermanent {
                assert!(provider.credential.read().await.is_none());
            } else {
                assert!(provider.account_state().await.is_ok());
            }
        }

        let provider =
            ConfiguredOpenAiProvider::new(config(), credential("secret", "account"), None).unwrap();
        assert!(matches!(
            provider.refresh_auth().await,
            Err(ProviderError::Credential(AuthError::RecoveryUnavailable))
        ));
        provider.disconnect_auth().await.unwrap();
        assert!(matches!(
            provider.account_state().await,
            Err(ProviderError::Credential(AuthError::RecoveryUnavailable))
        ));
    }
}
