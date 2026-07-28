//! Offline provider conformance suite (EP-001/US-004).
//!
//! Each fixture is a golden derived from the Codex baseline contract: either a
//! canonical request with the exact Responses body it must produce, or an SSE
//! stream with the exact canonical events it must yield. A wire regression on
//! function or custom tools fails here, with no network and no account.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use agent_core::model::ResponsesDialect;
use agent_core::provider::{CanonicalRequest, StreamEvent};
use agent_provider::chatgpt_events::{CodexEventMapper, MAPPED_OUTPUT_ITEM_TYPES};
use agent_provider::chatgpt_request::{ResponsesBodyOptions, build_responses_body};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    #[allow(dead_code)]
    note: String,
    #[serde(default)]
    request: Option<RequestCase>,
    #[serde(default)]
    stream: Option<StreamCase>,
}

#[derive(Debug, Deserialize)]
struct RequestCase {
    canonical: CanonicalRequest,
    options: FixtureOptions,
    body: Value,
}

#[derive(Debug, Deserialize)]
struct FixtureOptions {
    reasoning_effort: Option<String>,
    include_encrypted_reasoning: bool,
    parallel_tool_calls: bool,
    text_verbosity: Option<String>,
    dialect: ResponsesDialect,
}

#[derive(Debug, Deserialize)]
struct StreamCase {
    events: Vec<Value>,
    expected: Vec<StreamEvent>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance")
}

fn load_fixtures() -> Vec<(PathBuf, String, Fixture)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("conformance fixtures directory")
        .map(|entry| entry.expect("readable entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "the suite must not silently be empty");
    files
        .into_iter()
        .map(|path| {
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
            let fixture: Fixture = serde_json::from_str(&body)
                .unwrap_or_else(|error| panic!("{} parses: {error}", path.display()));
            (path, body, fixture)
        })
        .collect()
}

#[test]
fn every_fixture_covers_exactly_one_contract() {
    for (path, _, fixture) in load_fixtures() {
        assert!(
            fixture.request.is_some() ^ fixture.stream.is_some(),
            "{} must declare either a request or a stream case",
            path.display()
        );
    }
}

#[test]
fn requests_match_their_baseline_body() {
    for (path, _, fixture) in load_fixtures() {
        let Some(case) = fixture.request else {
            continue;
        };
        case.canonical.validate().unwrap_or_else(|error| {
            panic!("{}: canonical request is invalid: {error}", fixture.name)
        });
        let options = ResponsesBodyOptions {
            reasoning_effort: case.options.reasoning_effort.as_deref(),
            include_encrypted_reasoning: case.options.include_encrypted_reasoning,
            parallel_tool_calls: case.options.parallel_tool_calls,
            text_verbosity: case.options.text_verbosity.as_deref(),
            dialect: case.options.dialect,
        };
        let body = build_responses_body(&case.canonical, options);
        assert_eq!(
            body,
            case.body,
            "{} ({}) diverged from its baseline body",
            fixture.name,
            path.display()
        );
    }
}

#[test]
fn streams_match_their_baseline_events() {
    for (path, _, fixture) in load_fixtures() {
        let Some(case) = fixture.stream else {
            continue;
        };
        let mut mapper = CodexEventMapper::new();
        let mut produced = Vec::new();
        for (index, event) in case.events.iter().enumerate() {
            let mapped = mapper.ingest(&event.to_string()).unwrap_or_else(|error| {
                panic!(
                    "{} ({}): event {index} failed: {error}",
                    fixture.name,
                    path.display()
                )
            });
            produced.extend(mapped);
        }
        assert_eq!(
            produced,
            case.expected,
            "{} ({}) diverged from its baseline events",
            fixture.name,
            path.display()
        );
    }
}

/// An output item type the mapper does not project would disappear from the
/// stream. The suite refuses to host one silently, and names it with its
/// position.
#[test]
fn unmapped_output_items_fail_with_their_type_and_position() {
    for (path, _, fixture) in load_fixtures() {
        let Some(case) = fixture.stream else {
            continue;
        };
        for (index, event) in case.events.iter().enumerate() {
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind != "response.output_item.added" && kind != "response.output_item.done" {
                continue;
            }
            let item_type = event
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!(
                        "{} ({}): item at position {index} has no type",
                        fixture.name,
                        path.display()
                    )
                });
            assert!(
                MAPPED_OUTPUT_ITEM_TYPES.contains(&item_type),
                "{} ({}): unmapped Responses item `{item_type}` at position {index}; map it in \
                 chatgpt_events.rs or the stream loses it",
                fixture.name,
                path.display()
            );
        }
    }
}

/// Fixtures ship in the repository: they must never carry a credential, an
/// account identifier or real session content.
#[test]
fn fixtures_contain_no_secret_or_account_data() {
    const FORBIDDEN: &[&str] = &[
        "Bearer ",
        "sk-",
        "eyJ",
        "access_token",
        "id_token",
        "refresh_token",
        "authorization",
        "chatgpt_account_id",
        "session_token",
        "@gmail.com",
        "strivex.fr",
    ];
    for (path, body, _) in load_fixtures() {
        let haystack = body.to_lowercase();
        for needle in FORBIDDEN {
            assert!(
                !haystack.contains(&needle.to_lowercase()),
                "{} contains `{needle}`",
                path.display()
            );
        }
    }
}
