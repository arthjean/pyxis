use std::time::Duration;

use agent_core::auxiliary::compact::{MemorySummarizeInput, RawMemory, RawMemoryMetadata};
use agent_core::provider::AuxiliaryCapabilities;
use serde_json::{Value, json};

use super::super::AuxiliaryCase;
use super::support::{
    HttpResponse, assert_http_error, assert_timeout, assert_unsupported, auxiliary, make_provider,
    make_provider_with_timeout, one_response_server, stalling_server,
};

pub(super) async fn assert_fixture(name: &str, case: &AuxiliaryCase) {
    let disabled = make_provider("http://127.0.0.1:9/v1/", AuxiliaryCapabilities::default());
    assert_unsupported(
        auxiliary(&disabled)
            .summarize_memories(&request())
            .await
            .unwrap_err(),
        "memories",
    );

    let (base, server) = one_response_server(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: json!({"output": [{
            "trace_summary": "trace summary",
            "memory_summary": "memory summary",
        }]})
        .to_string()
        .into_bytes(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    let output = auxiliary(&provider)
        .summarize_memories(&request())
        .await
        .unwrap();
    let capture = server.await.unwrap();
    let body: Value = serde_json::from_slice(&capture.body).unwrap();
    let actual = json!({
        "path": capture.target,
        "trace_id": body["traces"][0]["id"],
        "trace_summary": output[0].raw_memory,
        "memory_summary": output[0].memory_summary,
    });
    assert_eq!(actual, case.success, "{name}: memories success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider)
            .summarize_memories(&request())
            .await
            .unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let error = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).summarize_memories(&request()),
    )
    .await
    .expect("memories timeout fixture exceeded its bound")
    .unwrap_err();
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        memories: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn request() -> MemorySummarizeInput {
    MemorySummarizeInput {
        model: "test-model".into(),
        raw_memories: vec![RawMemory {
            id: "trace-1".into(),
            metadata: RawMemoryMetadata {
                source_path: "/fixture/trace.json".into(),
            },
            items: vec![json!({"type": "message", "role": "user", "content": []})],
        }],
        reasoning: None,
    }
}
