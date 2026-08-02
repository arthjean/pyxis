use std::time::Duration;

use agent_core::auxiliary::compact::{CompactRequest, apply_after_durable_commit};
use agent_core::provider::AuxiliaryCapabilities;
use serde_json::{Value, json};

use super::super::AuxiliaryCase;
use super::support::{
    HttpResponse, assert_http_error, assert_timeout, assert_unsupported, auxiliary, make_provider,
    make_provider_with_timeout, one_response_server, response_item, stalling_server,
};

pub(super) async fn assert_fixture(name: &str, case: &AuxiliaryCase) {
    let disabled = make_provider("http://127.0.0.1:9/v1/", AuxiliaryCapabilities::default());
    assert_unsupported(
        auxiliary(&disabled)
            .compact_remote(&request())
            .await
            .unwrap_err(),
        "remote_compact",
    );

    let (base, server) = one_response_server(HttpResponse {
        status: 200,
        headers: vec![("x-codex-turn-state".into(), "turn-state-1".into())],
        body: json!({"output": [{
            "type": "message",
            "id": "msg_compact",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "summary"}],
        }]})
        .to_string()
        .into_bytes(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    let compact = auxiliary(&provider)
        .compact_remote(&request())
        .await
        .unwrap();
    let capture = server.await.unwrap();
    let body: Value = serde_json::from_slice(&capture.body).unwrap();

    let mut live = vec![response_item(json!({
        "type": "message",
        "id": "msg_old",
        "role": "user",
        "content": [{"type": "input_text", "text": "old"}],
    }))];
    let before = serde_json::to_vec(&live).unwrap();
    let failed = apply_after_durable_commit(&mut live, compact.clone(), |_| {
        std::future::ready(Err::<(), _>("durable write failed"))
    })
    .await;
    assert!(failed.is_err());
    assert_eq!(serde_json::to_vec(&live).unwrap(), before);
    let turn_state = apply_after_durable_commit(&mut live, compact, |candidate| {
        assert_eq!(candidate.turn_state.as_deref(), Some("turn-state-1"));
        std::future::ready(Ok::<(), &str>(()))
    })
    .await
    .unwrap();
    let actual = json!({
        "path": capture.target,
        "auth": capture.headers.contains_key("authorization"),
        "input_type": body["input"][0]["type"],
        "turn_state": turn_state,
        "output_kind": live[0].kind().wire_type(),
        "durable_before_memory": true,
    });
    assert_eq!(actual, case.success, "{name}: compact success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider)
            .compact_remote(&request())
            .await
            .unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let error = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).compact_remote(&request()),
    )
    .await
    .expect("compact timeout fixture exceeded its bound")
    .unwrap_err();
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        remote_compact: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn request() -> CompactRequest {
    CompactRequest {
        model: "test-model".into(),
        input: vec![response_item(json!({
            "type": "message",
            "id": "msg_input",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}],
        }))],
        instructions: "Keep decisions.".into(),
        tools: None,
        parallel_tool_calls: true,
        reasoning: None,
        service_tier: Some("default".into()),
        prompt_cache_key: Some("cache-fixture".into()),
        text: None,
    }
}
