//! Scoped remote model catalog refresh shared by Responses providers.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_auth::provider::ProviderRequestAuth;
use agent_core::provider::{ProviderError, StreamEvent};
use futures_util::{StreamExt, stream::BoxStream};

use crate::chatgpt_error::from_http_parts;
use crate::models::{CatalogModel, CatalogScope, ModelCatalog};

const MODELS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogCacheMode {
    Revalidate,
    Bypass,
}

#[derive(Clone)]
pub(crate) struct CatalogRequest {
    auth: ProviderRequestAuth,
    scope: CatalogScope,
    epoch: u64,
}

impl CatalogRequest {
    pub(crate) fn new(
        provider: impl Into<String>,
        auth: ProviderRequestAuth,
        identity_fingerprint: String,
        epoch: u64,
    ) -> Result<Self, ProviderError> {
        let scope = CatalogScope {
            provider: provider.into(),
            endpoint: normalized_endpoint(&auth.url)?,
            identity_fingerprint,
        };
        Ok(Self { auth, scope, epoch })
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> &CatalogScope {
        &self.scope
    }

    #[cfg(test)]
    pub(crate) fn url(&self) -> &str {
        &self.auth.url
    }
}

#[derive(Clone)]
pub(crate) struct RemoteCatalogManager {
    http: reqwest::Client,
    catalog: Arc<RwLock<ModelCatalog>>,
    refresh_gate: Arc<tokio::sync::Mutex<()>>,
    scope_epoch: Arc<AtomicU64>,
}

impl RemoteCatalogManager {
    pub(crate) fn new(
        http: reqwest::Client,
        catalog: Arc<RwLock<ModelCatalog>>,
        refresh_gate: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            http,
            catalog,
            refresh_gate,
            scope_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn scope_epoch(&self) -> u64 {
        self.scope_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn invalidate_scope(&self) -> Result<(), ProviderError> {
        let mut catalog = self
            .catalog
            .write()
            .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))?;
        self.scope_epoch.fetch_add(1, Ordering::AcqRel);
        catalog.clear_remote();
        Ok(())
    }

    pub(crate) async fn refresh(
        &self,
        request: CatalogRequest,
        cache: CatalogCacheMode,
        etag_hint: Option<String>,
    ) -> Result<Vec<CatalogModel>, ProviderError> {
        let _refresh = self.refresh_gate.lock().await;
        let cached_etag = {
            let mut catalog = self
                .catalog
                .write()
                .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))?;
            self.ensure_current(request.epoch)?;
            catalog.ensure_scope(request.scope.clone());
            catalog.etag().map(str::to_string)
        };
        if cache == CatalogCacheMode::Revalidate
            && etag_hint.as_deref().is_some()
            && cached_etag.as_deref() == etag_hint.as_deref()
        {
            return self.models_for_epoch(request.epoch);
        }

        let mut builder = self
            .http
            .get(&request.auth.url)
            .timeout(MODELS_TIMEOUT)
            .header(reqwest::header::ACCEPT, "application/json");
        for (name, value) in &request.auth.headers {
            builder = builder.header(name, value.expose());
        }
        if cache == CatalogCacheMode::Revalidate
            && let Some(etag) = cached_etag.as_deref()
        {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = builder
            .send()
            .await
            .map_err(|_| ProviderError::Transport("models request failed".into()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let response_etag = headers
            .get("x-models-etag")
            .or_else(|| headers.get(reqwest::header::ETAG))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .or(etag_hint);
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return self.models_for_epoch(request.epoch);
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ProviderError::Transport("models body failed".into()))?;
            if body.len().saturating_add(chunk.len()) > MAX_MODELS_BODY {
                return Err(ProviderError::Decode(format!(
                    "models: response exceeds {MAX_MODELS_BODY} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body)
            .map_err(|_| ProviderError::Decode("models: invalid UTF-8".into()))?;
        if !status.is_success() {
            return Err(from_http_parts(status.as_u16(), &headers, &body));
        }
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let mut catalog = self
            .catalog
            .write()
            .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))?;
        self.ensure_current(request.epoch)?;
        let models = catalog
            .install_remote_scoped(&body, &fetched_at, request.scope, response_etag)
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

    pub(crate) fn observe<F, Fut>(
        &self,
        stream: BoxStream<'static, Result<StreamEvent, ProviderError>>,
        request_factory: F,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>>
    where
        F: Fn() -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<CatalogRequest, ProviderError>> + Send + 'static,
    {
        let manager = self.clone();
        stream
            .map(move |event| {
                let etag = match &event {
                    Ok(StreamEvent::ResponseMetadata { metadata }) => metadata.models_etag.clone(),
                    _ => None,
                };
                if let Some(etag) = etag.filter(|etag| manager.etag_changed(etag)) {
                    let manager = manager.clone();
                    let request_factory = request_factory.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let request = request_factory().await?;
                            manager
                                .refresh(request, CatalogCacheMode::Revalidate, Some(etag))
                                .await
                        }
                        .await;
                        if let Err(error) = result {
                            tracing::warn!(
                                target: "pyxis::models",
                                error = %error,
                                "ETag-driven model catalog refresh failed"
                            );
                        }
                    });
                }
                event
            })
            .boxed()
    }

    #[cfg(test)]
    fn models(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        self.catalog
            .read()
            .map(|catalog| catalog.models())
            .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))
    }

    fn models_for_epoch(&self, epoch: u64) -> Result<Vec<CatalogModel>, ProviderError> {
        let catalog = self
            .catalog
            .read()
            .map_err(|_| ProviderError::Decode("models: catalog lock poisoned".into()))?;
        self.ensure_current(epoch)?;
        Ok(catalog.models())
    }

    fn etag_changed(&self, observed: &str) -> bool {
        self.catalog
            .read()
            .ok()
            .and_then(|catalog| catalog.etag().map(str::to_string))
            .as_deref()
            != Some(observed)
    }

    fn ensure_current(&self, expected: u64) -> Result<(), ProviderError> {
        if self.scope_epoch() != expected {
            return Err(ProviderError::Transport(
                "models request scope changed during refresh".into(),
            ));
        }
        Ok(())
    }
}

fn normalized_endpoint(raw: &str) -> Result<String, ProviderError> {
    let mut url = url::Url::parse(raw)
        .map_err(|_| ProviderError::Decode("models: invalid scoped endpoint".into()))?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn read_headers(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8(bytes).expect("request headers");
            }
        }
    }

    #[tokio::test]
    async fn remote_catalog_revalidates_bypasses_and_keeps_the_last_valid_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let fixture = include_str!("../../fixtures/models-2026-07-28.json").to_string();
        let server_fixture = fixture.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response_index in 0..4 {
                let (mut socket, _) = listener.accept().await.expect("accept");
                requests.push(read_headers(&mut socket).await);
                let (status, etag, body) = match response_index {
                    0 | 2 => ("200 OK", "etag-a", server_fixture.as_str()),
                    1 => ("304 Not Modified", "etag-a", ""),
                    _ => ("503 Service Unavailable", "", "unavailable"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nx-models-etag: {etag}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("respond");
            }
            requests
        });
        let catalog = Arc::new(RwLock::new(ModelCatalog::remote_only()));
        let manager = RemoteCatalogManager::new(
            reqwest::Client::new(),
            Arc::clone(&catalog),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let request = CatalogRequest::new(
            "fixture",
            ProviderRequestAuth {
                url: format!("http://{address}/models?client_version=current"),
                headers: vec![("authorization".into(), "Bearer catalog-token".into())],
            },
            "identity-fingerprint".into(),
            manager.scope_epoch(),
        )
        .expect("catalog request");
        assert!(!request.scope().endpoint.contains("client_version"));

        let first = manager
            .refresh(request.clone(), CatalogCacheMode::Revalidate, None)
            .await
            .expect("initial snapshot");
        assert!(!first.is_empty());
        assert_eq!(
            manager
                .refresh(request.clone(), CatalogCacheMode::Revalidate, None)
                .await
                .expect("304 snapshot"),
            first
        );
        assert_eq!(
            manager
                .refresh(request.clone(), CatalogCacheMode::Bypass, None)
                .await
                .expect("uncached snapshot"),
            first
        );
        assert!(
            manager
                .refresh(request, CatalogCacheMode::Revalidate, None)
                .await
                .is_err()
        );
        assert_eq!(manager.models().expect("stale snapshot"), first);

        let requests = server.await.expect("server");
        assert!(requests.iter().all(|request| {
            request.starts_with("GET /models?client_version=current HTTP/1.1")
                && request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer catalog-token")
        }));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("if-none-match: etag-a")
        );
        assert!(!requests[2].to_ascii_lowercase().contains("if-none-match"));
    }

    #[tokio::test]
    async fn scope_invalidation_prevents_an_inflight_refresh_from_reinstalling_old_models() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _request = read_headers(&mut socket).await;
            accepted_tx.send(()).expect("accepted signal");
            release_rx.await.expect("release response");
            let body = include_str!("../../fixtures/models-2026-07-28.json");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("respond");
        });
        let catalog = Arc::new(RwLock::new(ModelCatalog::remote_only()));
        let manager = RemoteCatalogManager::new(
            reqwest::Client::new(),
            Arc::clone(&catalog),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let request = CatalogRequest::new(
            "fixture",
            ProviderRequestAuth {
                url: format!("http://{address}/models"),
                headers: Vec::new(),
            },
            "old-identity".into(),
            manager.scope_epoch(),
        )
        .expect("request");
        let refreshing = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .refresh(request, CatalogCacheMode::Revalidate, None)
                    .await
            }
        });
        accepted_rx.await.expect("request accepted");
        manager.invalidate_scope().expect("scope invalidated");
        release_tx.send(()).expect("response released");
        let error = refreshing
            .await
            .expect("refresh joined")
            .expect_err("old refresh rejected");
        assert!(error.to_string().contains("scope changed"));
        assert!(catalog.read().expect("catalog").models().is_empty());
        server.await.expect("server");
    }
}
