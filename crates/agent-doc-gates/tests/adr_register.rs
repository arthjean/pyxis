//! The register gate (US-055, US-056): `docs/DECISIONS.md` holds what it says.
//!
//! The register carries a summary table maintained by hand, and that table had
//! already lost ADR-13 without anyone noticing, which is the exact mode of
//! failure the note tree exists to prevent. It also announces a per-decision
//! format whose alternatives section four records of thirteen never carried. Both
//! promises are read here, so a decision added without its row or without what it
//! beat stops `cargo test --workspace`.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a violation must
//! stop the suite with the offending identifier and the rule in the message, and
//! `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    ADR_ALTERNATIVES_HEADING, DECISIONS_DOC, DISPENSE_MARKER, check_decisions,
    check_decisions_document, repository_root,
};

/// One conforming decision, section and row, used as the neighbor of whatever a
/// fixture is proving.
fn decision(id: u32) -> String {
    format!(
        "## ADR-{id} — un sujet\n\n**Décision.** Le verdict.\n\n{ADR_ALTERNATIVES_HEADING} L'option battue et pourquoi.\n"
    )
}

fn register(rows: &[u32], sections: &[String]) -> String {
    let mut document = String::from("# Registre\n\n| ADR | Sujet | Statut |\n|---|---|---|\n");
    for id in rows {
        document.push_str(&format!("| ADR-{id} | un sujet | Accepté |\n"));
    }
    document.push('\n');
    for section in sections {
        document.push_str(section);
        document.push('\n');
    }
    document
}

#[test]
fn the_register_of_this_repository_lists_and_argues_every_decision_it_documents() {
    let violations = check_decisions(&repository_root());
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn a_decision_documented_without_its_summary_row_is_reported_by_its_identifier() {
    // The real defect this gate was written for: ADR-13 was a section its own
    // table never listed.
    let document = register(&[12], &[decision(12), decision(13)]);
    let violations = check_decisions_document(&document);
    assert_eq!(
        violations,
        vec!["decisions: ADR-13 absent du tableau récapitulatif"],
        "{violations:?}"
    );
}

#[test]
fn a_summary_row_without_its_section_is_reported_as_an_orphan_identifier() {
    let document = register(&[12, 13], &[decision(12)]);
    let violations = check_decisions_document(&document);
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("the orphan row is reported");
    assert!(reported.contains("ADR-13"), "{reported}");
    assert!(reported.contains("sans section"), "{reported}");
}

#[test]
fn a_hole_in_the_numbering_is_not_a_violation() {
    let document = register(&[1, 13], &[decision(1), decision(13)]);
    assert!(
        check_decisions_document(&document).is_empty(),
        "a retired identifier leaves a hole, and renumbering is impossible"
    );
}

#[test]
fn a_decision_that_never_says_what_it_beat_is_reported() {
    let document = register(
        &[7],
        &["## ADR-7 — un sujet\n\n**Décision.** Le verdict.\n".to_string()],
    );
    let violations = check_decisions_document(&document);
    assert_eq!(violations.len(), 1, "{violations:?}");
    let reported = violations.first().expect("the missing section is reported");
    assert!(reported.contains("ADR-7"), "{reported}");
    assert!(reported.contains("Alternatives écartées"), "{reported}");
}

#[test]
fn a_decision_declaring_its_alternatives_unrecoverable_is_accepted() {
    let document = register(
        &[7],
        &[format!(
            "## ADR-7 — un sujet\n\n**Décision.** Le verdict.\n\n{DISPENSE_MARKER}\n"
        )],
    );
    assert!(
        check_decisions_document(&document).is_empty(),
        "alternatives are recorded, never invented after the fact"
    );
}

#[test]
fn the_last_decision_of_the_document_is_checked_like_the_others() {
    // Nothing closes the last section but the end of the file: a gate that only
    // checked on the next heading would let the final decision through, and the
    // final decision is precisely where ADR-13 sat.
    let document = register(
        &[1, 13],
        &[
            decision(1),
            "## ADR-13 — un sujet\n\n**Décision.** Le verdict.\n".to_string(),
        ],
    );
    let violations = check_decisions_document(&document);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations
            .first()
            .expect("the last decision is reported")
            .contains("ADR-13"),
        "{violations:?}"
    );
}

#[test]
fn an_identifier_quoted_inside_a_fenced_block_is_not_read_as_a_section() {
    let document = register(
        &[1],
        &[format!(
            "{}\n```markdown\n## ADR-99 — un exemple\n| ADR-98 | un exemple | Accepté |\n```\n",
            decision(1)
        )],
    );
    assert!(
        check_decisions_document(&document).is_empty(),
        "a quoted heading shows a shape, it does not index a decision"
    );
}

#[test]
fn a_table_nested_inside_a_decision_does_not_index_it() {
    // ADR-7 carries a risk table and ADR-13 a table of measures. Only what
    // precedes the first decision is the summary table.
    let document = register(
        &[1],
        &[format!(
            "{}\n| Question | Mesure |\n|---|---|\n| ADR-42 | hors index |\n",
            decision(1)
        )],
    );
    assert!(
        check_decisions_document(&document).is_empty(),
        "a row inside a decision documents it, it does not list it"
    );
}

#[test]
fn an_unreadable_register_is_reported_without_panicking() {
    let violations = check_decisions(&repository_root().join("nulle-part"));
    let reported = violations.first().expect("the unreadable file is reported");
    assert!(reported.contains(DECISIONS_DOC), "{reported}");
    assert!(reported.contains("illisible"), "{reported}");
}

#[test]
fn every_violation_names_its_identifier_on_a_single_line() {
    let document = register(&[9], &[decision(1)]);
    let violations = check_decisions_document(&document);
    assert!(!violations.is_empty());
    for reported in &violations {
        assert_eq!(reported.lines().count(), 1, "{reported}");
        assert!(reported.starts_with("decisions: ADR-"), "{reported}");
    }
}
