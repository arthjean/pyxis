use std::time::Duration;

use agent_core::provider::{AuthError, ProviderError, StreamEvent};
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

pub(super) fn cancellation_guarded<S>(
    mut inner: S,
    cancellation: CancellationToken,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Send + Unpin + 'static,
{
    async_stream::stream! {
        loop {
            let next = tokio::select! {
                _ = cancellation.cancelled() => {
                    yield Err(ProviderError::Credential(AuthError::ReconnectRequired));
                    return;
                }
                next = inner.next() => next,
            };
            match next {
                Some(item) => {
                    let stop = item.is_err();
                    yield item;
                    if stop {
                        return;
                    }
                }
                None => return,
            }
        }
    }
}

pub(crate) fn idle_guarded<S>(
    mut inner: S,
    idle: Duration,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Send + Unpin + 'static,
{
    async_stream::stream! {
        loop {
            match tokio::time::timeout(idle, inner.next()).await {
                Err(_) => {
                    yield Err(ProviderError::Stream("idle timeout".into()));
                    return;
                }
                Ok(None) => return,
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

pub(crate) async fn send_with_header_timeout(
    client: &reqwest::Client,
    request: reqwest::Request,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    tokio::time::timeout(timeout, client.execute(request))
        .await
        .map_err(|_| ProviderError::Stream("header timeout".into()))?
        .map_err(|error| ProviderError::Transport(error.to_string()))
}
