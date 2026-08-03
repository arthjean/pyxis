//! Provider failures: attempt budget, credential recovery and overload fallback.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::RunConfig;
use agent_core::message::Message;
use agent_core::provider::AuthError;
use agent_core::provider::ErrorClass;
use agent_core::provider::ProviderError;
use std::sync::Arc;

mod common;

use common::{BlockingClock, MockTurn, RefreshBehavior, drive, harness, text_turn};

#[tokio::test]
async fn auth_expired_refreshes_then_retries_opening_stream() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Http {
                status: 401,
                message: "backend wording without an expiry marker".into(),
                retry_after_ms: None,
            }),
            text_turn("ok"),
        ],
        false,
        100_000,
    );
    let refreshes = Arc::clone(&h.refreshes);
    let log = Arc::clone(&h.log);
    let ctx = AgentContext::new("mock").push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    assert_eq!(*refreshes.lock().unwrap(), 1);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        2
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CredentialRefresh(view)
            if view.outcome == agent_core::CredentialRefreshOutcome::Started
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CredentialRefresh(view)
            if view.outcome == agent_core::CredentialRefreshOutcome::Succeeded
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled(view)
            if view.ordinal == 2
                && view.cause == ErrorClass::Auth(AuthError::Expired)
                && view.delay_ms == 0
    )));
}

#[tokio::test]
async fn persistent_401_refreshes_once_then_requires_reconnection() {
    let unauthorized = || {
        MockTurn::Err(ProviderError::Http {
            status: 401,
            message: "unauthorized".into(),
            retry_after_ms: None,
        })
    };
    let h = harness(
        vec![unauthorized(), unauthorized(), text_turn("must not open")],
        false,
        100_000,
    );
    let refreshes = Arc::clone(&h.refreshes);
    let log = Arc::clone(&h.log);
    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;

    assert_eq!(*refreshes.lock().unwrap(), 1);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        2,
        "the persistent 401 must not open a third provider attempt"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Auth(
            AuthError::ReconnectRequired
        )))
    ));
}

#[tokio::test]
async fn cancellation_during_refresh_starts_no_provider_retry() {
    let h = harness(
        vec![MockTurn::Err(ProviderError::Http {
            status: 401,
            message: "unauthorized".into(),
            retry_after_ms: None,
        })],
        false,
        100_000,
    );
    *h.refresh_behavior.lock().unwrap() = RefreshBehavior::Block;
    let started = Arc::clone(&h.refresh_started);
    let log = Arc::clone(&h.log);
    let cancel = h.deps.cancel.clone();
    let task = tokio::spawn(drive(
        AgentContext::new("mock").push(Message::user("go")),
        h.deps,
    ));

    started.acquire().await.unwrap().forget();
    cancel.cancel();
    let events = tokio::time::timeout(std::time::Duration::from_millis(100), task)
        .await
        .expect("refresh cancellation must return within 100 ms")
        .unwrap();

    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CredentialRefresh(view)
            if view.outcome == agent_core::CredentialRefreshOutcome::Cancelled
    )));
    assert!(matches!(events.last(), Some(AgentEvent::Interrupted(..))));
}

#[tokio::test]
async fn cancellation_during_backoff_starts_no_provider_retry() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Transport("temporary cut".into())),
            text_turn("must not open"),
        ],
        false,
        100_000,
    );
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let log = Arc::clone(&h.log);
    let cancel = h.deps.cancel.clone();
    let mut deps = h.deps;
    deps.clock = Arc::new(BlockingClock {
        started: Arc::clone(&started),
    });
    let task = tokio::spawn(drive(
        AgentContext::new("mock").push(Message::user("go")),
        deps,
    ));

    started.acquire().await.unwrap().forget();
    cancel.cancel();
    let events = tokio::time::timeout(std::time::Duration::from_millis(100), task)
        .await
        .expect("backoff cancellation must return within 100 ms")
        .unwrap();

    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| **entry == "stream")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::RetryScheduled(_)))
    );
    assert!(matches!(events.last(), Some(AgentEvent::Interrupted(..))));
}

#[tokio::test]
async fn rejected_refresh_requires_reconnection_without_exposing_provider_body() {
    let h = harness(
        vec![MockTurn::Err(ProviderError::Http {
            status: 401,
            message: "unauthorized".into(),
            retry_after_ms: None,
        })],
        false,
        100_000,
    );
    *h.refresh_behavior.lock().unwrap() = RefreshBehavior::Reject;
    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
    let encoded = serde_json::to_string(&events).unwrap();

    assert!(!encoded.contains("refresh rejected"));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CredentialRefresh(view)
            if view.outcome == agent_core::CredentialRefreshOutcome::Rejected
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Auth(
            AuthError::ReconnectRequired
        )))
    ));
}

#[tokio::test]
async fn typed_recovery_failures_survive_the_sampling_boundary() {
    for (expected, outcome) in [
        (
            AuthError::RecoveryPermanent,
            agent_core::CredentialRefreshOutcome::Permanent,
        ),
        (
            AuthError::RecoveryTransient,
            agent_core::CredentialRefreshOutcome::Transient,
        ),
        (
            AuthError::RecoveryUnavailable,
            agent_core::CredentialRefreshOutcome::Unavailable,
        ),
    ] {
        let h = harness(
            vec![MockTurn::Err(ProviderError::Http {
                status: 401,
                message: "unauthorized".into(),
                retry_after_ms: None,
            })],
            false,
            100_000,
        );
        *h.refresh_behavior.lock().unwrap() = RefreshBehavior::Auth(expected);
        let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CredentialRefresh(view) if view.outcome == outcome
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Error(agent_core::AgentError::Auth(actual))) if *actual == expected
        ));
    }
}

#[tokio::test]
async fn overload_opening_stream_switches_to_configured_fallback_model() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Http {
                status: 529,
                message: "overloaded".into(),
                retry_after_ms: None,
            }),
            text_turn("ok"),
        ],
        false,
        100_000,
    );
    let request_models = Arc::clone(&h.request_models);
    let ctx = AgentContext::new("primary")
        .with_config(RunConfig {
            overload_fallback_model: Some("fallback".into()),
            ..RunConfig::default()
        })
        .push(Message::user("go"));
    let events = drive(ctx, h.deps).await;
    assert!(matches!(events.last(), Some(AgentEvent::EndTurn)));
    assert_eq!(
        *request_models.lock().unwrap(),
        vec!["primary".to_string(), "fallback".to_string()]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled(view)
            if view.ordinal == 2
                && view.fallback_model.as_deref() == Some("fallback")
    )));
}

#[tokio::test]
async fn fallback_keeps_the_original_attempt_budget_and_terminal_taxonomy() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Http {
                status: 529,
                message: "overloaded".into(),
                retry_after_ms: None,
            }),
            MockTurn::Err(ProviderError::Transport("fallback unavailable".into())),
            text_turn("must not open"),
        ],
        false,
        100_000,
    );
    let requests = Arc::clone(&h.request_models);
    let events = drive(
        AgentContext::new("primary")
            .with_config(RunConfig {
                max_retries: 1,
                overload_fallback_model: Some("fallback".into()),
                ..RunConfig::default()
            })
            .push(Message::user("go")),
        h.deps,
    )
    .await;

    assert_eq!(
        *requests.lock().unwrap(),
        vec!["primary".to_string(), "fallback".to_string()]
    );
    let retries: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::RetryScheduled(view) => Some(view),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].ordinal, 2);
    assert_eq!(retries[0].max_attempts, 2);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Provider(failure)))
            if failure.class == Some(ErrorClass::Retryable)
    ));
}

#[tokio::test]
async fn context_recovery_cannot_exceed_the_total_attempt_budget() {
    let h = harness(
        vec![
            MockTurn::Err(ProviderError::Transport("temporary cut".into())),
            MockTurn::Err(ProviderError::ContextLengthExceeded),
            text_turn("must not open"),
        ],
        false,
        100_000,
    );
    let requests = Arc::clone(&h.request_models);
    let events = drive(
        AgentContext::new("model")
            .with_config(RunConfig {
                max_retries: 1,
                ..RunConfig::default()
            })
            .push(Message::user("go")),
        h.deps,
    )
    .await;

    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::RetryScheduled(_)))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Provider(failure)))
            if failure.class == Some(ErrorClass::ContextLimit)
    ));
}

#[tokio::test]
async fn terminal_provider_details_are_redacted_at_the_public_event_boundary() {
    let secret = "Bearer AT_SECRET account_id=acct_secret";
    let h = harness(
        vec![MockTurn::Err(ProviderError::Stream(secret.into()))],
        false,
        100_000,
    );
    let events = drive(
        AgentContext::new("model")
            .with_config(RunConfig {
                max_retries: 0,
                ..RunConfig::default()
            })
            .push(Message::user("go")),
        h.deps,
    )
    .await;
    let encoded = serde_json::to_string(&events).unwrap();

    assert!(!encoded.contains("AT_SECRET"));
    assert!(!encoded.contains("acct_secret"));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(agent_core::AgentError::Provider(failure)))
            if failure.class == Some(ErrorClass::Retryable)
                && failure.message == "provider stream failed"
    ));
}
