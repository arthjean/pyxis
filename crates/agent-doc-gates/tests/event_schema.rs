//! The event-schema gate (US-127): `docs/EVENT_SCHEMA.md` says what
//! `AgentEvent` is, and its examples are lines the binary really produced.
//!
//! The document is the contract an integrator writes a parser from, and it had
//! drifted in two directions at once: six variants had no row, and every
//! example carried identifier prefixes (`th_`, `tu_`, `ev_`) the binary never
//! emits. Neither is visible to a reader, because a document that describes
//! nothing checkable reads exactly like a document that is right.
//!
//! So the two halves are proved apart. The count and the names catch a variant
//! added without its row; the byte comparison against the frozen transcripts of
//! US-126 catches an example that was written rather than observed. A block no
//! scenario can produce is admitted, on the sole condition that it says so
//! above itself: `turn_diff` needs a git repository and no scenario runs in
//! one, and pretending otherwise would be the same lie in the other direction.
//!
//! The gate launches no process, opens no socket and reads no environment
//! variable. It reads two files of this repository plus the transcripts its
//! anchors name, which is why it belongs inside `cargo test --workspace`.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a drifted
//! document must stop the suite with the gap in the message, and
//! `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    EVENT_SCHEMA_DOC, EVENT_SOURCE, NON_VARIANT_ROWS, TRANSCRIPT_ANCHOR, UNFROZEN_ANCHOR,
    check_event_schema, check_event_types, check_examples, documented_types, examples,
    repository_root, variant_names,
};
use std::fs;

/// The published document of this repository.
fn document() -> String {
    fs::read_to_string(repository_root().join(EVENT_SCHEMA_DOC))
        .expect("the event schema is readable")
}

/// The source it is compared to.
fn source() -> String {
    fs::read_to_string(repository_root().join(EVENT_SOURCE)).expect("the event source is readable")
}

// The document of this repository.

#[test]
fn every_variant_of_agent_event_has_a_line_in_the_published_schema() {
    let violations = check_event_schema(&repository_root());
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn the_six_types_the_document_used_to_omit_are_documented() {
    let documented = documented_types(&document()).expect("the table parses");
    for silent in [
        "hook",
        "plan",
        "response_metadata",
        "response_item",
        "provider_extension",
        "unmapped_response_item",
    ] {
        assert!(
            documented.iter().any(|entry| entry == silent),
            "`{silent}` is absent from {EVENT_SCHEMA_DOC}"
        );
    }
}

#[test]
fn the_published_table_lists_the_variants_in_the_order_the_enumeration_declares_them() {
    let variants = variant_names(&source()).expect("the enumeration parses");
    let documented: Vec<String> = documented_types(&document())
        .expect("the table parses")
        .into_iter()
        .filter(|entry| !NON_VARIANT_ROWS.contains(&entry.as_str()))
        .collect();
    assert_eq!(documented, variants);
}

#[test]
fn every_example_of_the_document_is_a_frozen_transcript_line_or_says_why_it_is_not() {
    let document = document();
    let found = examples(&document);
    assert!(
        found.len() > 8,
        "the document lost its examples: {} left",
        found.len()
    );
    let anchored = found
        .iter()
        .filter(|example| {
            example
                .anchor
                .as_deref()
                .is_some_and(|anchor| anchor.starts_with(TRANSCRIPT_ANCHOR))
        })
        .count();
    assert!(
        anchored >= found.len() - 3,
        "only {anchored} of {} examples are drawn from a transcript",
        found.len()
    );
    assert!(check_examples(&repository_root(), &document).is_empty());
}

// What the parser reads out of the source.

#[test]
fn the_fields_of_a_struct_variant_never_pass_for_variants_of_their_own() {
    let parsed = variant_names(
        "pub enum AgentEvent {\n\
         /// A doc comment holding a { brace and a ( paren.\n\
         StreamReset,\n\
         Text(String),\n\
         ReasoningReplayDisabled {\n\
         reason: String,\n\
         Nested: u8,\n\
         },\n\
         ResponseItem {\n\
         #[serde(default)]\n\
         OutputIndex: Option<u64>,\n\
         },\n\
         EndTurn,\n\
         }\n",
    )
    .expect("the enumeration parses");
    assert_eq!(
        parsed,
        vec![
            "stream_reset",
            "text",
            "reasoning_replay_disabled",
            "response_item",
            "end_turn"
        ]
    );
}

#[test]
fn an_enumeration_that_never_closes_fails_instead_of_reporting_what_it_read() {
    let error = variant_names("pub enum AgentEvent {\n    StreamReset,\n")
        .expect_err("an unterminated enumeration is refused");
    assert!(error.contains("AgentEvent"), "{error}");
    assert!(error.contains(EVENT_SOURCE), "{error}");
}

#[test]
fn a_source_without_the_enumeration_fails_instead_of_comparing_zero_variants() {
    let error =
        variant_names("pub struct Autre {}\n").expect_err("an absent enumeration is refused");
    assert!(error.contains(EVENT_SOURCE), "{error}");
}

// What the parser reads out of the document.

#[test]
fn the_header_row_of_the_table_is_not_read_as_a_documented_type() {
    let parsed = documented_types(
        "## Types d'événements\n\
         \n\
         | `type` | `data` | Signification |\n\
         |---|---|---|\n\
         | `text` | chaîne | Un fragment. |\n\
         | `end_turn` | — | La fin. |\n\
         \n\
         Une phrase après la table.\n",
    )
    .expect("the table parses");
    assert_eq!(parsed, vec!["text", "end_turn"]);
}

#[test]
fn a_document_without_the_section_fails_instead_of_finding_no_type() {
    let error = documented_types("# Titre\n\nUne phrase.\n")
        .expect_err("a document without the section is refused");
    assert!(error.contains(EVENT_SCHEMA_DOC), "{error}");
}

// What the comparison reports.

#[test]
fn a_variant_added_without_its_row_names_the_variant_and_the_two_counts() {
    let variants = ["text".to_string(), "hook".to_string()];
    let documented = ["text".to_string()];
    let violations = check_event_types(&variants, &documented);
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(
        violations[0].contains("2 variantes, 1 types documentés"),
        "{violations:?}"
    );
    assert!(violations[1].contains("`hook`"), "{violations:?}");
}

#[test]
fn a_row_that_matches_no_variant_is_reported_unless_it_is_declared_as_coming_from_elsewhere() {
    let variants = ["text".to_string()];
    let declared = ["text".to_string(), NON_VARIANT_ROWS[0].to_string()];
    assert!(check_event_types(&variants, &declared).is_empty());

    let undeclared = ["text".to_string(), "invente".to_string()];
    let violations = check_event_types(&variants, &undeclared);
    assert!(
        violations.iter().any(|entry| entry.contains("`invente`")),
        "{violations:?}"
    );
}

// What the example comparison refuses.

#[test]
fn a_json_block_without_a_marker_is_refused() {
    let violations = check_examples(
        &repository_root(),
        "## Titre\n\n```json\n{\"schema\":1}\n```\n",
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains(TRANSCRIPT_ANCHOR), "{violations:?}");
    assert!(violations[0].contains(UNFROZEN_ANCHOR), "{violations:?}");
}

#[test]
fn an_unfrozen_marker_without_a_reason_is_refused() {
    let violations = check_examples(
        &repository_root(),
        "<!-- hors transcription: -->\n```json\n{\"schema\":1}\n```\n",
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("raison"), "{violations:?}");
}

#[test]
fn an_anchored_example_that_no_longer_matches_its_transcript_reports_both_lines() {
    let violations = check_examples(
        &repository_root(),
        "<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:8 -->\n\
         ```json\n\
         {\"schema\":1,\"type\":\"run_summary\",\"data\":{\"end\":\"end_turn\"}}\n\
         ```\n",
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("gelé :"), "{violations:?}");
    assert!(violations[0].contains("publié :"), "{violations:?}");
    assert!(violations[0].contains("bare-turn"), "{violations:?}");
}

#[test]
fn an_anchor_aimed_at_a_line_or_a_file_that_does_not_exist_is_reported() {
    let missing_line = check_examples(
        &repository_root(),
        "<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:900 -->\n\
         ```json\n{}\n```\n",
    );
    assert_eq!(missing_line.len(), 1, "{missing_line:?}");
    assert!(missing_line[0].contains("ligne 900"), "{missing_line:?}");

    let missing_file = check_examples(
        &repository_root(),
        "<!-- transcription: crates/agent-cli/tests/transcripts/absent/expected.jsonl:1 -->\n\
         ```json\n{}\n```\n",
    );
    assert_eq!(missing_file.len(), 1, "{missing_file:?}");
    assert!(missing_file[0].contains("illisible"), "{missing_file:?}");
}

#[test]
fn the_gate_launches_no_process_and_reads_no_environment_variable_of_its_own() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/event_schema.rs"),
    )
    .expect("the module source is readable");
    for forbidden in ["std::process", "Command", "env::var", "env!"] {
        assert!(
            !source.contains(forbidden),
            "the event-schema gate must stay a pure read of two files: {forbidden}"
        );
    }
}
