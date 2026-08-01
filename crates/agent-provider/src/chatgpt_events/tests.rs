use super::*;

fn ingest_all(events: &[&str]) -> Vec<StreamEvent> {
    let mut m = CodexEventMapper::new();
    let mut out = Vec::new();
    for e in events {
        out.extend(m.ingest(e).unwrap());
    }
    out
}

#[test]
fn text_delta_maps() {
    let ev = ingest_all(&[r#"{"type":"response.output_text.delta","delta":"Hello"}"#]);
    assert_eq!(
        ev,
        vec![StreamEvent::TextDelta {
            text: "Hello".into()
        }]
    );
}

#[test]
fn reasoning_deltas_map() {
    let ev = ingest_all(&[
        r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        r#"{"type":"response.reasoning_text.delta","delta":" encore"}"#,
    ]);
    assert_eq!(
        ev,
        vec![
            StreamEvent::ReasoningDelta {
                text: "thinking".into()
            },
            StreamEvent::ReasoningDelta {
                text: " encore".into()
            },
        ]
    );
}

#[test]
fn completed_without_tools_is_endturn_with_usage() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_text.delta","delta":"ok"}"#,
        r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":120,"output_tokens":8}}}"#,
    ]);
    assert!(ev.contains(&StreamEvent::Usage {
        usage: TokenUsage::new(120, 8)
    }));
    assert_eq!(
        ev.last(),
        Some(&StreamEvent::Done {
            stop: StopReason::EndTurn
        })
    );
}

#[test]
fn completed_end_turn_false_requests_continuation() {
    let ev = ingest_all(&[
        r#"{"type":"response.completed","response":{"status":"completed","end_turn":false}}"#,
    ]);
    assert_eq!(
        ev,
        [StreamEvent::Done {
            stop: StopReason::Continue
        }]
    );
}

#[test]
fn missing_end_turn_keeps_legacy_end_turn_behavior() {
    let ev = ingest_all(&[r#"{"type":"response.completed","response":{"status":"completed"}}"#]);
    assert_eq!(
        ev,
        [StreamEvent::Done {
            stop: StopReason::EndTurn
        }]
    );
}

#[test]
fn incomplete_reasons_are_distinct() {
    for (reason, expected) in [
        ("max_output_tokens", StopReason::MaxTokens),
        ("content_filter", StopReason::ContentFilter),
        ("future_reason", StopReason::IncompleteUnknown),
    ] {
        let event = format!(
            r#"{{"type":"response.incomplete","response":{{"status":"incomplete","incomplete_details":{{"reason":"{reason}"}}}}}}"#
        );
        let ev = ingest_all(&[&event]);
        assert_eq!(ev.last(), Some(&StreamEvent::Done { stop: expected }));
    }
}

#[test]
fn invalid_end_turn_and_contradictory_terminals_fail_closed() {
    for event in [
        r#"{"type":"response.completed","response":{"status":"completed","end_turn":"false"}}"#,
        r#"{"type":"response.completed","response":{"status":"incomplete"}}"#,
        r#"{"type":"response.incomplete","response":{"status":"incomplete","end_turn":false}}"#,
    ] {
        let mut mapper = CodexEventMapper::new();
        assert!(matches!(
            mapper.ingest(event),
            Err(ProviderError::Decode(_))
        ));
    }
}

#[test]
fn function_call_full_lifecycle_reassembles_valid_json() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"bash","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"cmd\":\""}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":"ls\"}"}"#,
        r#"{"type":"response.function_call_arguments.done","arguments":"{\"cmd\":\"ls\"}"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_7","id":"fc_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":50,"output_tokens":12}}}"#,
    ]);

    assert!(ev.contains(&StreamEvent::tool_call_start("call_7", "bash")));
    assert!(ev.contains(&StreamEvent::ToolCallEnd {
        id: "call_7".into()
    }));
    // stop = ToolUse because a tool call was emitted.
    assert_eq!(
        ev.last(),
        Some(&StreamEvent::Done {
            stop: StopReason::ToolUse
        })
    );

    // invariant: concatenated input_delta = valid JSON.
    let args: String = ev
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, input_delta } if id == "call_7" => {
                Some(input_delta.clone())
            }
            _ => None,
        })
        .collect();
    let parsed: serde_json::Value = serde_json::from_str(&args).expect("JSON valide");
    assert_eq!(parsed["cmd"], "ls");
}

#[test]
fn interleaved_function_calls_are_tracked_by_item_id() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"path\":\""}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"path\":\""}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"a.txt\"}"}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"b.txt\"}"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":"{\"path\":\"b.txt\"}"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ]);

    let args_for = |call_id: &str| -> String {
        ev.iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, input_delta } if id == call_id => {
                    Some(input_delta.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(args_for("call_a"), "{\"path\":\"a.txt\"}");
    assert_eq!(args_for("call_b"), "{\"path\":\"b.txt\"}");
    assert_eq!(
        ev.last(),
        Some(&StreamEvent::Done {
            stop: StopReason::ToolUse
        })
    );
}

#[test]
fn ambiguous_parallel_tool_delta_fails_closed() {
    let mut m = CodexEventMapper::new();
    assert!(
            m.ingest(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#
            )
            .unwrap()
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { id, .. } if id == "call_a"))
        );
    assert!(
            m.ingest(
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#
            )
            .unwrap()
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallStart { id, .. } if id == "call_b"))
        );
    let err = m
        .ingest(r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#)
        .unwrap_err();
    assert!(matches!(err, ProviderError::Decode(_)));
}

#[test]
fn parallel_tool_delta_can_fallback_to_unique_call_id() {
    let mut m = CodexEventMapper::new();
    m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
        )
        .unwrap();
    m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","id":"fc_b","name":"write","arguments":""}}"#,
        )
        .unwrap();
    assert!(
            m.ingest(
                r#"{"type":"response.function_call_arguments.done","call_id":"call_b","arguments":"{\"path\":\"b.txt\"}"}"#
            )
            .unwrap()
            .is_empty()
        );
    let ev = m
            .ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"write"}}"#,
            )
            .unwrap();
    assert!(ev.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ToolCallDelta { id, input_delta }
            if id == "call_b" && input_delta == "{\"path\":\"b.txt\"}"
        )
    }));
}

#[test]
fn args_only_in_item_done_still_emitted() {
    // backend that sends no deltas: args only in output_item.done.
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","id":"fc","name":"x","arguments":""}}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","id":"fc","name":"x","arguments":"{\"a\":1}"}}"#,
    ]);
    let args: String = ev
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { input_delta, .. } => Some(input_delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"a\":1}");
}

#[test]
fn function_call_done_without_added_reconstructs_call() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ]);
    assert!(ev.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ToolCallStart { id, name, .. } if id == "c1" && name == "read"
        )
    }));
    assert!(ev.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ToolCallDelta { id, input_delta }
            if id == "c1" && input_delta == "{\"path\":\"Cargo.toml\"}"
        )
    }));
    assert_eq!(
        ev.last(),
        Some(&StreamEvent::Done {
            stop: StopReason::ToolUse
        })
    );
}

#[test]
fn function_call_done_without_added_or_name_fails_closed() {
    let mut m = CodexEventMapper::new();
    let err = m
            .ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","arguments":"{}"}}"#,
            )
            .unwrap_err();
    assert!(matches!(err, ProviderError::Decode(_)));
}

#[test]
fn incomplete_status_without_a_reason_fails_closed_as_unknown() {
    let ev = ingest_all(&[r#"{"type":"response.incomplete","response":{"status":"incomplete"}}"#]);
    assert_eq!(
        ev,
        vec![StreamEvent::Done {
            stop: StopReason::IncompleteUnknown
        }]
    );
}

#[test]
fn error_event_yields_typed_error_not_panic() {
    let mut m = CodexEventMapper::new();
    let err = m
        .ingest(r#"{"type":"error","code":"server_error","message":"boom"}"#)
        .unwrap_err();
    assert!(matches!(err, ProviderError::Stream(_)));
}

#[test]
fn context_length_error_is_classified_for_withholding() {
    let mut m = CodexEventMapper::new();
    let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"maximum context length"}}}"#,
            )
            .unwrap_err();
    assert!(matches!(err, ProviderError::ContextLengthExceeded));
    assert!(err.is_context_error());
}

#[test]
fn response_failed_invalid_request_is_not_stream_retryable() {
    let mut m = CodexEventMapper::new();
    let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"invalid_request_error","message":"bad schema"}}}"#,
            )
            .unwrap_err();
    assert!(matches!(
        err,
        ProviderError::Http {
            status: 400,
            retry_after_ms: None,
            ..
        }
    ));
}

#[test]
fn response_failed_rate_limit_keeps_rate_limited_status() {
    let mut m = CodexEventMapper::new();
    let err = m
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"too many requests"}}}"#,
            )
            .unwrap_err();
    assert!(matches!(
        err,
        ProviderError::Http {
            status: 429,
            retry_after_ms: None,
            ..
        }
    ));
}

// Encrypted content stays replay-gated, while the non-sensitive reasoning
// state remains observable in both modes.
#[test]
fn reasoning_item_captured_only_when_replay_on() {
    let done = r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC"}}"#;
    let off = CodexEventMapper::new().ingest(done).unwrap();
    assert!(matches!(
        off.as_slice(),
        [StreamEvent::ResponseMetadata { metadata }]
            if metadata.reasoning.item_id.as_deref() == Some("rs_1")
    ));
    // ON -> metadata followed by the opaque replay item.
    let ev = CodexEventMapper::with_replay(true).ingest(done).unwrap();
    assert!(matches!(
        ev.as_slice(),
        [
            StreamEvent::ResponseMetadata { metadata },
            StreamEvent::EncryptedReasoning { id, encrypted_content }
        ] if metadata.reasoning.item_id.as_deref() == Some("rs_1")
            && id == "rs_1" && encrypted_content == "ENC"
    ));
    // Missing encrypted content keeps the metadata but emits no replay item.
    let empty = r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_2"}}"#;
    assert!(matches!(
        CodexEventMapper::with_replay(true)
            .ingest(empty)
            .unwrap()
            .as_slice(),
        [StreamEvent::ResponseMetadata { metadata }]
            if metadata.reasoning.item_id.as_deref() == Some("rs_2")
    ));
}

#[test]
fn malformed_chunk_is_typed_error() {
    let mut m = CodexEventMapper::new();
    assert!(matches!(
        m.ingest("{not json").unwrap_err(),
        ProviderError::Decode(_)
    ));
    assert!(matches!(
        m.ingest("").unwrap_err(),
        ProviderError::Decode(_)
    ));
}

#[test]
fn an_item_id_is_never_reported_as_the_response_id() {
    let mut mapper = CodexEventMapper::new();
    let events = mapper
        .ingest(r#"{"type":"response.output_text.delta","id":"item_1","delta":"hello"}"#)
        .unwrap();
    assert_eq!(
        events,
        vec![StreamEvent::TextDelta {
            text: "hello".into()
        }]
    );
}

#[test]
fn known_lifecycle_events_stay_quiet_while_unknown_events_remain_observable() {
    let mut mapper = CodexEventMapper::new();
    for event_type in KNOWN_UNPROJECTED_EVENTS {
        let event = serde_json::json!({"type": event_type});
        assert!(mapper.ingest(&event.to_string()).unwrap().is_empty());
    }

    let unknown = mapper.ingest(r#"{"type":"response.future"}"#).unwrap();
    assert!(matches!(
        unknown.as_slice(),
        [StreamEvent::ProviderExtension { extension }]
            if extension.event_type() == "response.future"
    ));
}

#[test]
fn fragmented_custom_tool_call_yields_one_terminal_call_with_text_input() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":""}}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"// @exec: cell\n"}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"const x = 1;"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":"// @exec: cell\nconst x = 1;"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ]);
    assert_eq!(
        ev,
        vec![
            StreamEvent::custom_tool_call_start("call_1", "exec"),
            StreamEvent::ToolCallDelta {
                id: "call_1".into(),
                input_delta: "// @exec: cell\nconst x = 1;".into(),
            },
            StreamEvent::ToolCallEnd {
                id: "call_1".into()
            },
            StreamEvent::Done {
                stop: StopReason::ToolUse
            },
        ]
    );
}

/// Duplicated deltas then an authoritative terminal item: the terminal
/// input wins and nothing is emitted twice.
#[test]
fn duplicate_deltas_lose_against_the_terminal_item() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":""}}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"print(1)"}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"print(1)"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_1","id":"ctc_1","name":"exec","input":"print(1)"}}"#,
    ]);
    let deltas: Vec<&str> = ev
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta { input_delta, .. } => Some(input_delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["print(1)"], "terminal item is authoritative");
    assert_eq!(
        ev.iter()
            .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
            .count(),
        1,
        "one dispatch, not two"
    );
}

#[test]
fn custom_tool_call_done_without_added_is_reconstructed() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"c1","name":"apply_patch","input":"*** Begin Patch"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ]);
    assert_eq!(
        ev.first(),
        Some(&StreamEvent::custom_tool_call_start("c1", "apply_patch"))
    );
    assert!(ev.contains(&StreamEvent::ToolCallDelta {
        id: "c1".into(),
        input_delta: "*** Begin Patch".into(),
    }));
}

#[test]
fn impossible_custom_streams_fail_closed_without_dispatch() {
    // Terminal item without a name: nothing is dispatchable.
    let mut m = CodexEventMapper::new();
    assert!(matches!(
            m.ingest(
                r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"c1","input":"x"}}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

    // A custom delta addressed to a function call would silently corrupt
    // its JSON arguments, and the reverse would corrupt freeform text.
    let mut m = CodexEventMapper::new();
    m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","id":"fc_a","name":"read","arguments":""}}"#,
        )
        .unwrap();
    assert!(matches!(
        m.ingest(
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"fc_a","delta":"raw"}"#,
        )
        .unwrap_err(),
        ProviderError::Decode(_)
    ));

    let mut m = CodexEventMapper::new();
    m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_a","name":"exec","input":""}}"#,
        )
        .unwrap();
    assert!(matches!(
        m.ingest(
            r#"{"type":"response.function_call_arguments.delta","item_id":"ctc_a","delta":"{}"}"#,
        )
        .unwrap_err(),
        ProviderError::Decode(_)
    ));

    // Format flip between the opening and the terminal item.
    let mut m = CodexEventMapper::new();
    m.ingest(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"c2","id":"ctc_2","name":"exec","input":""}}"#,
        )
        .unwrap();
    assert!(matches!(
            m.ingest(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","id":"ctc_2","name":"exec","arguments":"{}"}}"#,
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));

    // Invalid UTF-8 escape inside the payload: rejected at decode, so no
    // call is ever built from it.
    let mut m = CodexEventMapper::new();
    assert!(matches!(
            m.ingest(
                "{\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"c3\",\"name\":\"exec\",\"input\":\"\\ud800\"}}",
            )
            .unwrap_err(),
            ProviderError::Decode(_)
        ));
}

#[test]
fn function_and_custom_calls_interleave_without_crosstalk() {
    let ev = ingest_all(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_f","id":"fc_1","name":"read","arguments":""}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_1","name":"exec","input":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"a.txt\"}"}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"await tools.read()"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_f","id":"fc_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_c","id":"ctc_1","name":"exec","input":"await tools.read()"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ]);
    let payload = |call: &str| -> Option<String> {
        ev.iter().find_map(|event| match event {
            StreamEvent::ToolCallDelta { id, input_delta } if id == call => {
                Some(input_delta.clone())
            }
            _ => None,
        })
    };
    assert_eq!(payload("call_f").as_deref(), Some(r#"{"path":"a.txt"}"#));
    assert_eq!(payload("call_c").as_deref(), Some("await tools.read()"));
    assert!(ev.contains(&StreamEvent::tool_call_start("call_f", "read")));
    assert!(ev.contains(&StreamEvent::custom_tool_call_start("call_c", "exec")));
}
