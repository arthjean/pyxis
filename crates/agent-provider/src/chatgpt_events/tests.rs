use super::*;

fn ingest_all(events: &[&str]) -> Vec<StreamEvent> {
    let mut mapper = CodexEventMapper::new();
    events
        .iter()
        .flat_map(|event| mapper.ingest(event).expect("event maps"))
        .collect()
}

fn extensions(events: &[StreamEvent]) -> Vec<&ProviderExtension> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ProviderExtension { extension } => Some(extension),
            _ => None,
        })
        .collect()
}

#[test]
fn text_delta_maps_without_extra_events() {
    assert_eq!(
        ingest_all(&[r#"{"type":"response.output_text.delta","delta":"Hello"}"#]),
        [StreamEvent::TextDelta {
            text: "Hello".into()
        }]
    );
}

#[test]
fn reasoning_lifecycle_stays_distinct_without_artificial_newline() {
    let events = ingest_all(&[
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0,"part":{"type":"summary_text","text":""}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"thinking"}"#,
        r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"thinking"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs_1","summary_index":0,"part":{"type":"summary_text","text":"thinking"}}"#,
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","content_index":1,"delta":"details"}"#,
        r#"{"type":"response.reasoning_text.done","item_id":"rs_1","content_index":1,"text":"details"}"#,
    ]);
    let reasoning: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, ["thinking", "details"]);
    assert!(!reasoning.iter().any(|text| text.contains('\n')));
    let types: Vec<&str> = extensions(&events)
        .into_iter()
        .map(ProviderExtension::event_type)
        .collect();
    assert_eq!(
        types,
        [
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.reasoning_text.delta",
            "response.reasoning_text.done",
        ]
    );
}

#[test]
fn fragmented_function_deltas_are_immediate_and_terminal_input_is_authoritative() {
    let mut mapper = CodexEventMapper::new();
    let added = mapper
        .ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"shell","arguments":""}}"#)
        .unwrap();
    assert!(matches!(added.last(), Some(StreamEvent::ToolCallStart { id, .. }) if id == "call_7"));

    for (delta, expected) in [(r#"{"cmd":""#, r#"{"cmd":""#), (r#"ls"}"#, r#"ls"}"#)] {
        let event = format!(
            r#"{{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":{}}}"#,
            serde_json::to_string(delta).unwrap()
        );
        assert_eq!(
            mapper.ingest(&event).unwrap(),
            [StreamEvent::ToolCallDelta {
                id: "call_7".into(),
                input_delta: expected.into(),
            }]
        );
    }
    let done = mapper
        .ingest(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"shell","arguments":"{\"cmd\":\"pwd\"}"}}"#)
        .unwrap();
    assert!(done.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallInputDone { id, input }
            if id == "call_7" && input == "{\"cmd\":\"pwd\"}"
    )));
    assert!(
        !done
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolCallDelta { .. }))
    );
    assert!(matches!(done.last(), Some(StreamEvent::ToolCallEnd { id }) if id == "call_7"));
}

#[test]
fn fragmented_custom_tool_input_is_emitted_at_arrival() {
    let events = ingest_all(&[
        r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"custom_1","id":"ct_1","name":"patch","input":""}}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ct_1","delta":"one"}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ct_1","delta":" two"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"custom_1","id":"ct_1","name":"patch","input":"authoritative"}}"#,
    ]);
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta { input_delta, .. } => Some(input_delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["one", " two"]);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallInputDone { input, .. } if input == "authoritative"
    )));
}

#[test]
fn parallel_calls_are_correlated_without_crosstalk_and_ambiguity_fails_closed() {
    let mut mapper = CodexEventMapper::new();
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_f","id":"fc_1","name":"read","arguments":""}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"custom_tool_call","call_id":"call_c","id":"ct_1","name":"exec","input":""}}"#,
    ] {
        mapper.ingest(event).unwrap();
    }
    assert!(matches!(
        mapper.ingest(r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#),
        Err(ProviderError::Decode(_))
    ));

    let function = mapper
        .ingest(
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"a.txt\"}"}"#,
        )
        .unwrap();
    let custom = mapper
        .ingest(
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"ct_1","delta":"run"}"#,
        )
        .unwrap();
    assert!(matches!(
        function.as_slice(),
        [StreamEvent::ToolCallDelta { id, input_delta }]
            if id == "call_f" && input_delta == "{\"path\":\"a.txt\"}"
    ));
    assert!(matches!(
        custom.as_slice(),
        [StreamEvent::ToolCallDelta { id, input_delta }]
            if id == "call_c" && input_delta == "run"
    ));
}

#[test]
fn terminal_items_reconstruct_complete_calls_but_reject_missing_identity() {
    for (item_type, input_field, input, expected_format) in [
        (
            "function_call",
            "arguments",
            r#"{"path":"Cargo.toml"}"#,
            ToolCallFormat::Json,
        ),
        (
            "custom_tool_call",
            "input",
            "*** Begin Patch",
            ToolCallFormat::Text,
        ),
    ] {
        let event = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": item_type,
                "call_id": "call_1",
                "name": "tool",
                (input_field): input,
            }
        });
        let events = CodexEventMapper::new().ingest(&event.to_string()).unwrap();
        assert!(matches!(
            events.first(),
            Some(StreamEvent::ToolCallStart { id, format, .. })
                if id == "call_1" && *format == expected_format
        ));
        assert!(matches!(
            events.get(1),
            Some(StreamEvent::ToolCallInputDone { id, input: value })
                if id == "call_1" && value == input
        ));
        assert!(matches!(events.last(), Some(StreamEvent::ToolCallEnd { id }) if id == "call_1"));
    }

    let error = CodexEventMapper::new()
        .ingest(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","arguments":"{}"}}"#,
        )
        .unwrap_err();
    assert!(matches!(error, ProviderError::Decode(_)));
}

#[test]
fn contradictory_tool_and_response_terminals_fail_closed() {
    let mut mapper = CodexEventMapper::new();
    mapper
        .ingest(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_1","id":"ct_1","name":"exec","input":""}}"#,
        )
        .unwrap();
    assert!(matches!(
        mapper.ingest(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","id":"ct_1","name":"exec","arguments":"{}"}}"#,
        ),
        Err(ProviderError::Decode(_))
    ));

    for event in [
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"failed"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","end_turn":"yes"}}"#,
    ] {
        assert!(matches!(
            CodexEventMapper::new().ingest(event),
            Err(ProviderError::Decode(_))
        ));
    }
}

#[test]
fn completed_publishes_metadata_usage_then_one_success_terminal() {
    let events = ingest_all(&[
        r#"{"type":"response.completed","request_id":"req_1","response":{"id":"resp_1","status":"completed","end_turn":false,"usage":{"input_tokens":4294967297,"output_tokens":4294967298,"total_tokens":8589934595}}}"#,
    ]);
    assert!(matches!(
        &events[0],
        StreamEvent::ResponseMetadata { metadata }
            if metadata.response_id.as_deref() == Some("resp_1")
                && metadata.request_id.as_deref() == Some("req_1")
                && metadata.end_turn == Some(false)
    ));
    assert_eq!(
        events[1],
        StreamEvent::Usage {
            usage: TokenUsage {
                input: 4_294_967_297,
                output: 4_294_967_298,
                total: 8_589_934_595,
                ..TokenUsage::default()
            }
        }
    );
    assert_eq!(
        events[2],
        StreamEvent::Done {
            stop: StopReason::Continue
        }
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Done { .. }))
            .count(),
        1
    );

    let mut mapper = CodexEventMapper::new();
    mapper
        .ingest(
            r#"{"type":"response.completed","response":{"id":"resp_once","status":"completed"}}"#,
        )
        .unwrap();
    assert!(matches!(
        mapper.ingest(
            r#"{"type":"response.completed","response":{"id":"resp_twice","status":"completed"}}"#,
        ),
        Err(ProviderError::Decode(_))
    ));
}

#[test]
fn usage_rejects_negative_and_out_of_i64_counters_at_the_provider_boundary() {
    for input_tokens in [serde_json::json!(-1), serde_json::json!(u64::MAX)] {
        let event = serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 1,
                    "total_tokens": 1
                }
            }
        });
        assert!(matches!(
            CodexEventMapper::new().ingest(&event.to_string()),
            Err(ProviderError::Decode(_))
        ));
    }
}

#[test]
fn usage_without_backend_total_falls_back_to_input_plus_output() {
    let events = ingest_all(&[
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":7,"output_tokens":5}}}"#,
    ]);
    let usage = TokenUsage {
        input: 7,
        output: 5,
        ..TokenUsage::default()
    };
    assert_eq!(events.get(1), Some(&StreamEvent::Usage { usage }));
    assert_eq!(usage.total(), 12);
}

#[test]
fn incomplete_failed_and_done_never_synthesize_success() {
    let cases = [
        (
            r#"{"type":"response.incomplete","response":{"id":"resp_i","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
            ProviderErrorCategory::Incomplete,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"resp_f","status":"failed","error":{"code":"invalid_request_error","message":"bad schema"}}}"#,
            ProviderErrorCategory::InvalidRequest,
        ),
        (
            r#"{"type":"response.done","response":{"id":"resp_d","status":"completed"}}"#,
            ProviderErrorCategory::Failed,
        ),
    ];
    for (event, expected) in cases {
        let error = CodexEventMapper::new().ingest(event).unwrap_err();
        assert!(matches!(error, ProviderError::Api { category, .. } if category == expected));
    }
}

#[test]
fn provider_error_categories_retry_delay_and_diagnostic_ids_survive() {
    let cases = [
        (
            "context_length_exceeded",
            ProviderErrorCategory::ContextOverflow,
        ),
        ("insufficient_quota", ProviderErrorCategory::Quota),
        (
            "usage_not_included",
            ProviderErrorCategory::UsageNotIncluded,
        ),
        ("cyber_policy_violation", ProviderErrorCategory::CyberPolicy),
        ("invalid_prompt", ProviderErrorCategory::InvalidPrompt),
        ("invalid_image", ProviderErrorCategory::InvalidImage),
        ("server_is_overloaded", ProviderErrorCategory::Overloaded),
        (
            "authentication_error",
            ProviderErrorCategory::Authentication,
        ),
        ("permission_denied", ProviderErrorCategory::PermissionDenied),
    ];
    for (code, expected) in cases {
        let event = serde_json::json!({
            "type": "response.failed",
            "request_id": "req_diag",
            "auth_request_id": "auth_diag",
            "response": {"error": {"code": code, "message": "provider failure"}}
        });
        let error = CodexEventMapper::new()
            .ingest(&event.to_string())
            .unwrap_err();
        assert!(
            matches!(error, ProviderError::Api { category, request_id: Some(ref request), auth_request_id: Some(ref auth), .. }
            if category == expected && request == "req_diag" && auth == "auth_diag")
        );
    }

    let error = CodexEventMapper::new()
        .ingest(r#"{"type":"error","code":"rate_limit_exceeded","message":"Try again in 11.054s","request_id":"req_rate"}"#)
        .unwrap_err();
    assert!(matches!(error, ProviderError::Api {
        category: ProviderErrorCategory::RateLimited,
        retry_after_ms: Some(11_054),
        request_id: Some(ref request),
        ..
    } if request == "req_rate"));
}

#[test]
fn diagnostic_ids_only_come_from_known_bounded_envelope_fields() {
    let nested = serde_json::json!({
        "type": "response.failed",
        "payload": {"auth_request_id": "nested-user-value"},
        "response": {"error": {"code": "server_error", "message": "boom"}}
    });
    assert!(matches!(
        CodexEventMapper::new().ingest(&nested.to_string()),
        Err(ProviderError::Api {
            auth_request_id: None,
            ..
        })
    ));

    let oversized = serde_json::json!({
        "type": "response.failed",
        "request_id": "x".repeat(257),
        "response": {"error": {"code": "server_error", "message": "boom"}}
    });
    assert!(matches!(
        CodexEventMapper::new().ingest(&oversized.to_string()),
        Err(ProviderError::Api {
            request_id: None,
            ..
        })
    ));
}

#[test]
fn malformed_json_is_diagnosed_and_later_events_continue() {
    let mut mapper = CodexEventMapper::new();
    let malformed = mapper.ingest("{broken").unwrap();
    assert!(matches!(
        &malformed[0],
        StreamEvent::ProviderExtension { extension }
            if extension.event_type() == "malformed_sse_event_ignored"
                && extension.payload()["reason"] == "invalid_json"
                && extension.payload().get("raw").is_none()
    ));
    assert_eq!(
        mapper
            .ingest(r#"{"type":"response.output_text.delta","delta":"still alive"}"#)
            .unwrap(),
        [StreamEvent::TextDelta {
            text: "still alive".into()
        }]
    );
    assert!(mapper
        .ingest(r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","end_turn":true}}"#)
        .unwrap()
        .iter()
        .any(|event| matches!(event, StreamEvent::Done { stop: StopReason::EndTurn })));
}

#[test]
fn known_and_unknown_output_items_preserve_complete_bounded_payloads() {
    let events = ingest_all(&[
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"message","id":"msg_1","status":"in_progress","content":[]}}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"web_search_call","id":"ws_1","status":"completed"}}"#,
    ]);
    assert!(extensions(&events).iter().any(|extension| {
        extension.event_type() == "response.output_item.added"
            && extension.payload()["item"]["id"] == "msg_1"
            && extension.payload()["output_index"] == 2
    }));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::UnmappedItem { item_type, extension: Some(extension) }
            if item_type == "web_search_call"
                && extension.payload()["id"] == "ws_1"
    )));
}

#[test]
fn known_output_item_frames_without_an_item_remain_observable() {
    let events = ingest_all(&[
        r#"{"type":"response.output_item.added","output_index":2}"#,
        r#"{"type":"response.output_item.done","output_index":2}"#,
    ]);
    let types: Vec<&str> = extensions(&events)
        .into_iter()
        .map(ProviderExtension::event_type)
        .collect();
    assert_eq!(
        types,
        ["response.output_item.added", "response.output_item.done"]
    );
}

#[test]
fn encrypted_reasoning_is_gated_but_reasoning_metadata_is_not() {
    let event = r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","status":"completed","encrypted_content":"opaque"}}"#;
    let without_replay = CodexEventMapper::new().ingest(event).unwrap();
    assert!(
        !without_replay
            .iter()
            .any(|event| matches!(event, StreamEvent::EncryptedReasoning { .. }))
    );
    let with_replay = CodexEventMapper::with_replay(true).ingest(event).unwrap();
    assert!(with_replay.iter().any(|event| matches!(
        event,
        StreamEvent::EncryptedReasoning { id, encrypted_content }
            if id == "rs_1" && encrypted_content == "opaque"
    )));
}
