use std::sync::Arc;

use agent_auth::provider::{ProviderAuthTarget, ProviderCredential, ProviderRequestAuth};
use agent_core::provider::{AuthError, ProviderError, StreamEvent};
use futures_util::stream::BoxStream;

use super::auth::auth_config_error;
use super::{ConfiguredOpenAiConfig, ConfiguredOpenAiProvider, OpenAiCatalogPolicy};
use crate::models::CatalogModel;
use crate::responses::catalog::{CatalogCacheMode, CatalogRequest};

impl ConfiguredOpenAiProvider {
    pub async fn list_models(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        match &self.config.catalog {
            OpenAiCatalogPolicy::Static(_) => self
                .catalog
                .read()
                .map(|catalog| catalog.models())
                .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into())),
            OpenAiCatalogPolicy::Remote { models_path } => {
                self.catalog_manager
                    .refresh(
                        configured_catalog_request(
                            self.config.clone(),
                            Arc::clone(&self.credential),
                            models_path.clone(),
                            self.catalog_manager.scope_epoch(),
                        )
                        .await?,
                        CatalogCacheMode::Revalidate,
                        None,
                    )
                    .await
            }
        }
    }

    pub async fn list_models_uncached(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        match &self.config.catalog {
            OpenAiCatalogPolicy::Static(_) => self.list_models().await,
            OpenAiCatalogPolicy::Remote { models_path } => {
                self.catalog_manager
                    .refresh(
                        configured_catalog_request(
                            self.config.clone(),
                            Arc::clone(&self.credential),
                            models_path.clone(),
                            self.catalog_manager.scope_epoch(),
                        )
                        .await?,
                        CatalogCacheMode::Bypass,
                        None,
                    )
                    .await
            }
        }
    }

    pub(super) fn observe_catalog_etag(
        &self,
        stream: BoxStream<'static, Result<StreamEvent, ProviderError>>,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let OpenAiCatalogPolicy::Remote { models_path } = &self.config.catalog else {
            return stream;
        };
        let config = self.config.clone();
        let credential = Arc::clone(&self.credential);
        let models_path = models_path.clone();
        let catalog_manager = self.catalog_manager.clone();
        self.catalog_manager.observe(stream, move || {
            configured_catalog_request(
                config.clone(),
                Arc::clone(&credential),
                models_path.clone(),
                catalog_manager.scope_epoch(),
            )
        })
    }

    pub(super) fn catalog_window(&self, model: &str) -> Option<u32> {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.context_window(model.trim()))
    }
}

pub(super) async fn configured_catalog_request(
    config: ConfiguredOpenAiConfig,
    credential: Arc<tokio::sync::RwLock<Option<ProviderCredential>>>,
    models_path: String,
    scope_epoch: u64,
) -> Result<CatalogRequest, ProviderError> {
    let credential = credential
        .read()
        .await
        .clone()
        .ok_or(ProviderError::Credential(AuthError::RecoveryUnavailable))?;
    let endpoint = config.endpoint()?;
    let auth = credential
        .resolve(&ProviderAuthTarget::OpenAi {
            provider: config.auth_provider,
            endpoint,
            allow_unauthenticated: config.allow_unauthenticated(),
        })
        .map_err(auth_config_error)?;
    let mut models_endpoint = config.transport.endpoint_for_path(&models_path)?;
    if !models_endpoint
        .query_pairs()
        .any(|(name, _)| name == "client_version")
    {
        models_endpoint.query_pairs_mut().append_pair(
            "client_version",
            &agent_auth::oauth::openai_chatgpt::codex_client_version(),
        );
    }
    let mut headers = config.transport.configured_endpoint_headers()?;
    headers.extend(auth.headers().iter().cloned());
    CatalogRequest::new(
        config.name,
        ProviderRequestAuth {
            url: models_endpoint.to_string(),
            headers,
        },
        auth.identity_fingerprint,
        scope_epoch,
    )
}
