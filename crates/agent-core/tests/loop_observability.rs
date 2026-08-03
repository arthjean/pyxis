//! What the loop reports without changing its decisions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentContext;
use agent_core::AgentEvent;
use agent_core::RunConfig;
use agent_core::message::Message;
use agent_core::provider::StreamEvent;
use agent_core::provider::TokenUsage;

mod common;

use common::{drive, harness, model_turns, text_turn_with};

#[tokio::test]
async fn response_metadata_and_extensions_cross_the_core_event_boundary() {
    let metadata = agent_core::provider::ResponseMetadata {
        response_id: Some("resp_1".into()),
        request_id: Some("req_1".into()),
        ..agent_core::provider::ResponseMetadata::default()
    };
    let extension = agent_core::provider::ProviderExtension::from_value(
        "response.future",
        serde_json::json!({"authorization": "Bearer secret", "detail": "kept"}),
    );
    let unmapped = agent_core::provider::ProviderExtension::from_value(
        "response.output_item.done:future_item",
        serde_json::json!({"type": "future_item", "value": 1}),
    );
    let h = harness(
        vec![text_turn_with(vec![
            StreamEvent::ResponseMetadata {
                metadata: Box::new(metadata.clone()),
            },
            StreamEvent::ProviderExtension {
                extension: extension.clone(),
            },
            StreamEvent::UnmappedItem {
                item_type: "future_item".into(),
                extension: Some(unmapped.clone()),
            },
        ])],
        false,
        10_000,
    );

    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ResponseMetadata(actual) if actual.as_ref() == &metadata)
    ));
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ProviderExtension(actual) if actual == &extension)
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::UnmappedResponseItem { item_type, extension: Some(actual) }
            if item_type == "future_item" && actual == &unmapped
    )));
    let encoded = serde_json::to_string(&events).unwrap();
    assert!(!encoded.contains("Bearer secret"));
    assert!(encoded.contains("[REDACTED]"));
}

/// US-002 AC1: the end-of-round-trip event carries the real occupancy of the
/// window and the window of the active model, next to the counters already
/// present.
#[tokio::test]
async fn model_turn_carries_backend_usage_and_model_window() {
    let h = harness(
        vec![text_turn_with(vec![StreamEvent::Usage {
            usage: TokenUsage::new(600, 5),
        }])],
        false,
        10_000,
    );
    let events = drive(
        AgentContext::new("windowed").push(Message::user("go")),
        h.deps,
    )
    .await;
    let turns = model_turns(&events);
    assert_eq!(turns.len(), 1, "{events:?}");
    assert_eq!(turns[0].context_tokens, Some(600));
    assert_eq!(turns[0].context_window, Some(2_000));
    assert_eq!(
        turns[0].estimated_context_tokens, None,
        "sonde inactive par défaut"
    );
}

/// US-002 AC3 and AC4: without a reported usage the measure is declared
/// absent instead of being reported as zero, and an unknown window stays
/// `None` so that no percentage can be computed in the core.
#[tokio::test]
async fn model_turn_reports_absent_measure_and_unknown_window() {
    let h = harness(vec![text_turn_with(Vec::new())], false, 10_000);
    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
    let turns = model_turns(&events);
    assert_eq!(turns.len(), 1, "{events:?}");
    assert_eq!(
        turns[0].context_tokens, None,
        "mesure absente, jamais rapportée à zéro"
    );
    assert_eq!(turns[0].context_window, None, "fenêtre inconnue");
    assert!(
        turns[0].input_tokens > 0,
        "les compteurs cumulés gardent leur repli estimé"
    );
}

/// US-002 AC5: the calibration probe is now data carried by the event; the
/// core computes it on demand and writes nothing.
#[tokio::test]
async fn usage_probe_travels_as_data_when_enabled() {
    let h = harness(
        vec![text_turn_with(vec![StreamEvent::Usage {
            usage: TokenUsage::new(600, 5),
        }])],
        false,
        10_000,
    );
    let ctx = AgentContext::new("windowed")
        .with_config(RunConfig {
            usage_probe: true,
            ..RunConfig::default()
        })
        .push(Message::user("go"));
    let turns = model_turns(&drive(ctx, h.deps).await);
    assert!(
        turns[0].estimated_context_tokens.is_some(),
        "sonde active: l'estimation locale accompagne la mesure"
    );
}

/// US-003 AC1: a quota state served by the provider becomes a structured
/// event; an empty state produces nothing at all (AC5).
#[tokio::test]
async fn quota_state_is_relayed_and_emptiness_is_silent() {
    let snapshot = agent_core::quota::QuotaSnapshot {
        primary: Some(agent_core::quota::QuotaWindow {
            used_percent: 42.0,
            window_minutes: Some(300),
            resets_at_unix: Some(1_784_989_920),
        }),
        ..agent_core::quota::QuotaSnapshot::default()
    };
    let h = harness(
        vec![text_turn_with(vec![StreamEvent::Quota { snapshot }])],
        false,
        10_000,
    );
    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
    let quotas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Quota(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(quotas.len(), 1, "{events:?}");
    assert_eq!(quotas[0].primary.map(|w| w.used_percent), Some(42.0));

    let h = harness(
        vec![text_turn_with(vec![StreamEvent::Quota {
            snapshot: agent_core::quota::QuotaSnapshot::default(),
        }])],
        false,
        10_000,
    );
    let events = drive(AgentContext::new("mock").push(Message::user("go")), h.deps).await;
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Quota(_))),
        "état vide: aucun événement émis ({events:?})"
    );
}
