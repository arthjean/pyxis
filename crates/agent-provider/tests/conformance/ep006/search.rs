use std::time::Duration;

use agent_core::auxiliary::search::{
    SearchCommands, SearchExternalWebAccess, SearchQuery, SearchRequest, SearchResponseLength,
    SearchSettings,
};
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
        auxiliary(&disabled).search(&request()).await.unwrap_err(),
        "search",
    );

    let (base, server) = one_response_server(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: json!({
            "encrypted_output": "ciphertext",
            "output": "result text",
            "results": [{"type": "text_result", "ref_id": "turn0search0"}],
            "items": [{
                "type": "message",
                "id": "msg_search",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "result text"}]
            }],
            "pagination": {"next_page": "cursor-2", "has_more": true},
        })
        .to_string()
        .into_bytes(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    let search = auxiliary(&provider).search(&request()).await.unwrap();
    let capture = server.await.unwrap();
    let body: Value = serde_json::from_slice(&capture.body).unwrap();
    let actual = json!({
        "path": capture.target,
        "command": body["commands"]["search_query"][0]["q"],
        "result_count": search.results.as_ref().unwrap().len(),
        "item_count": search.items.as_ref().unwrap().len(),
        "next_page": search.pagination.next_page,
    });
    assert_eq!(actual, case.success, "{name}: search success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider).search(&request()).await.unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let error = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).search(&request()),
    )
    .await
    .expect("search timeout fixture exceeded its bound")
    .unwrap_err();
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        search: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn request() -> SearchRequest {
    SearchRequest {
        id: "search-1".into(),
        model: "test-model".into(),
        reasoning: None,
        input: None,
        commands: Some(SearchCommands {
            search_query: Some(vec![SearchQuery {
                q: "Pyxis".into(),
                recency: None,
                domains: Some(vec!["example.test".into()]),
            }]),
            response_length: Some(SearchResponseLength::Short),
            ..SearchCommands::default()
        }),
        settings: Some(SearchSettings {
            external_web_access: Some(SearchExternalWebAccess::Boolean(true)),
            ..SearchSettings::default()
        }),
        max_output_tokens: Some(500),
    }
}
