//! The format gate (US-049, US-050): what a decision record has to look like.
//!
//! `docs/DECISIONS.md` announces a per-decision format in its own header and four
//! of its thirteen records diverge from it, silently, because nothing reads that
//! promise. This gate is what keeps the note tree from repeating it: the header
//! block, the status line crossed with the lifecycle directory, the section
//! skeleton, and the mandatory alternatives are all checked, and the
//! specification's own skeletons are run through the gate so the document cannot
//! describe a note the machine would reject.
//!
//! `panic!` through `assert!` is the reporting mechanism here: a violation must
//! stop `cargo test --workspace` with the offending path and the rule in the
//! message, and `clippy.toml`'s test exemptions do not reach integration tests.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    ALTERNATIVES_HEADING, BANNED_IN_IMPLEMENTED, CLASSES, DISPENSE_MARKER, FORMAT_ADOPTED,
    LIFECYCLES, Note, PROBLEM_HEADING, check_format, check_note_file, notes_root, repository_root,
    required_headings, walk_notes,
};
use std::fs;
use std::path::{Path, PathBuf};

/// A date after the adoption of the format, so a fixture needs real alternatives.
const AFTER_ADOPTION: &str = "2026-08-21";

/// A date before it, where the dispense marker is the honest way out.
const BEFORE_ADOPTION: &str = "2026-07-27";

const VALID_IMPLEMENTED: &str = "\
# Note: le sujet

Statut: implemented

## Problème

Le corps.

## Décision

Le verdict.

## Alternatives écartées

**Une autre voie.** Pourquoi elle a perdu.

## Conséquences

Ce que cela coûte.
";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the scratch tree is created");
    dir
}

fn readme() -> String {
    fs::read_to_string(notes_root().join("README.md")).expect("the specification is readable")
}

/// Every fenced `markdown` block of a document, fences excluded. The
/// specification carries one per lifecycle, and nothing else uses that info
/// string.
fn markdown_blocks(document: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    let mut inside = false;
    for line in document.lines() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            if inside {
                if let Some(block) = current.take() {
                    blocks.push(block.join("\n"));
                }
                inside = false;
            } else {
                inside = true;
                if info.trim() == "markdown" {
                    current = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push(line);
        }
    }
    blocks
}

#[test]
fn the_repository_note_tree_conforms_to_the_documented_format() {
    let (notes, structure) = walk_notes(&notes_root());
    assert!(structure.is_empty(), "{}", structure.join("\n"));
    let violations: Vec<String> = notes.iter().flat_map(check_note_file).collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn the_skeletons_of_the_specification_are_accepted_by_the_gate_without_modification() {
    let blocks = markdown_blocks(&readme());
    assert_eq!(
        blocks.len(),
        LIFECYCLES.len(),
        "the specification shows one skeleton per lifecycle"
    );
    for (lifecycle, block) in LIFECYCLES.iter().zip(&blocks) {
        assert!(
            block.contains(&format!("Statut: {lifecycle}")),
            "skeleton {lifecycle} announces another lifecycle:\n{block}"
        );
        let violations = check_format(lifecycle, "README.md", AFTER_ADOPTION, block);
        assert!(
            violations.is_empty(),
            "the {lifecycle} skeleton is refused by the gate it describes: {}",
            violations.join("\n")
        );
    }
}

#[test]
fn the_specification_states_every_value_and_section_the_gate_enforces() {
    let readme = readme();
    let mut required: Vec<String> = vec![
        "# Note: <titre>".to_string(),
        PROBLEM_HEADING.to_string(),
        ALTERNATIVES_HEADING.to_string(),
        DISPENSE_MARKER.to_string(),
        FORMAT_ADOPTED.to_string(),
        "INDEX.md".to_string(),
        "aaaa-mm-jj-sujet.md".to_string(),
        "README.md".to_string(),
        ".md".to_string(),
    ];
    required.extend(LIFECYCLES.iter().map(|value| format!("`{value}/`")));
    required.extend(CLASSES.iter().map(|value| format!("`{value}`")));
    // Derived, not transcribed: extending a heading set in the crate without
    // writing the new rule down is the drift this whole tree exists to stop, and a
    // hand-kept list here would be one more index diverging in silence.
    required.extend(BANNED_IN_IMPLEMENTED.iter().map(ToString::to_string));
    required.extend(
        LIFECYCLES
            .iter()
            .flat_map(|lifecycle| required_headings(lifecycle))
            .map(ToString::to_string),
    );
    for token in required {
        assert!(
            readme.contains(&token),
            "no gate rule exists without its written counterpart: « {token} » is missing from docs/notes/README.md"
        );
    }
}

#[test]
fn a_conforming_note_is_accepted() {
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, VALID_IMPLEMENTED);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn a_note_whose_first_line_is_not_the_title_template_fails() {
    let content = VALID_IMPLEMENTED.replace("# Note: le sujet", "# Le sujet");
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(
        violations.iter().any(|line| line.contains("la ligne 1")),
        "{violations:?}"
    );
}

#[test]
fn a_note_whose_second_line_is_not_blank_fails() {
    let content = VALID_IMPLEMENTED.replace(
        "# Note: le sujet\n\n",
        "# Note: le sujet\nune ligne de trop\n",
    );
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(
        violations.iter().any(|line| line.contains("la ligne 2")),
        "{violations:?}"
    );
}

#[test]
fn a_note_whose_fourth_line_is_not_blank_fails() {
    let content = VALID_IMPLEMENTED.replace(
        "Statut: implemented\n\n",
        "Statut: implemented\ncollé au statut\n",
    );
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(
        violations.iter().any(|line| line.contains("la ligne 4")),
        "{violations:?}"
    );
}

#[test]
fn an_implemented_note_whose_status_announces_another_lifecycle_fails_naming_the_disagreement() {
    let content = VALID_IMPLEMENTED.replace("Statut: implemented", "Statut: proposed");
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    let reported = violations
        .iter()
        .find(|line| line.contains("statut incompatible"))
        .expect("the disagreement is reported");
    assert!(reported.contains("implemented/"), "{reported}");
    assert!(reported.contains("Statut: proposed"), "{reported}");
}

#[test]
fn a_rejected_note_whose_status_carries_no_reason_fails() {
    let content = "\
# Note: le sujet

Statut: rejected

## Problème

Le corps.

## Proposition

Ce qui était proposé.

## Alternatives écartées

**Une autre voie.** Pourquoi elle a perdu.
";
    let violations = check_format("rejected", "note.md", AFTER_ADOPTION, content);
    assert!(
        violations.iter().any(|line| line.contains("sa raison")),
        "{violations:?}"
    );

    let with_reason = content.replace(
        "Statut: rejected",
        "Statut: rejected - le coût dépasse le gain",
    );
    let accepted = check_format("rejected", "note.md", AFTER_ADOPTION, &with_reason);
    assert!(accepted.is_empty(), "{}", accepted.join("\n"));
}

#[test]
fn a_note_whose_first_section_is_not_the_problem_heading_fails_citing_the_heading_found() {
    let content = VALID_IMPLEMENTED.replace("## Problème", "## Contexte");
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    let reported = violations
        .iter()
        .find(|line| line.contains("la première section"))
        .expect("the opening section is reported");
    assert!(reported.contains("## Contexte"), "{reported}");
}

#[test]
fn an_implemented_note_carrying_a_proposal_heading_fails_naming_the_offending_heading() {
    let content = VALID_IMPLEMENTED.replace(
        "## Décision",
        "## Plan de migration\n\nÀ venir.\n\n## Décision",
    );
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    let reported = violations
        .iter()
        .find(|line| line.contains("titre de proposition"))
        .expect("the proposal heading is reported");
    assert!(reported.contains("## Plan de migration"), "{reported}");
}

#[test]
fn a_heading_that_merely_begins_like_a_banned_one_is_accepted_and_the_real_one_is_reported_once() {
    // `docs/notes/README.md` names four banned titles and no prefix rule, so
    // « ## Planification » has to pass: a note may not be refused by a rule its
    // reader cannot find written down.
    let lookalike = VALID_IMPLEMENTED.replace(
        "## Conséquences",
        "## Planification capacitaire\n\nCe qui reste à dimensionner.\n\n## Conséquences",
    );
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &lookalike);
    assert!(violations.is_empty(), "{}", violations.join("\n"));

    let banned = VALID_IMPLEMENTED.replace(
        "## Conséquences",
        "## Plan de migration\n\nÀ venir.\n\n## Conséquences",
    );
    let reported: Vec<String> = check_format("implemented", "note.md", AFTER_ADOPTION, &banned)
        .into_iter()
        .filter(|line| line.contains("titre de proposition"))
        .collect();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(reported[0].contains("## Plan de migration"), "{reported:?}");
}

#[test]
fn a_note_missing_a_required_section_of_its_lifecycle_fails() {
    let content = VALID_IMPLEMENTED.replace("## Conséquences", "## Suites");
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("« ## Conséquences » manquante")),
        "{violations:?}"
    );
}

#[test]
fn a_note_without_alternatives_and_without_the_dated_dispense_fails() {
    let content = VALID_IMPLEMENTED.replace("## Alternatives écartées", "## Notes");
    let violations = check_format("implemented", "note.md", BEFORE_ADOPTION, &content);
    let reported = violations
        .iter()
        .find(|line| line.contains("« ## Alternatives écartées » manquante"))
        .expect("the missing alternatives are reported");
    assert!(reported.contains(FORMAT_ADOPTED), "{reported}");
}

#[test]
fn a_second_status_line_in_the_body_fails() {
    let content = VALID_IMPLEMENTED.replace("Le verdict.", "Statut: implemented");
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("la ligne de statut doit être unique")),
        "{violations:?}"
    );
}

#[test]
fn format_tokens_inside_a_fenced_block_are_ignored_and_the_note_still_passes() {
    let content = VALID_IMPLEMENTED.replace(
        "Le verdict.",
        "Le verdict.\n\n```markdown\n# Note: un exemple\n\nStatut: proposed\n\n## Contexte\n## Plan de migration\n```",
    );
    let violations = check_format("implemented", "note.md", AFTER_ADOPTION, &content);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn a_note_dated_before_the_adoption_carrying_the_dispense_passes_without_alternatives() {
    let content = VALID_IMPLEMENTED.replace(
        "## Alternatives écartées\n\n**Une autre voie.** Pourquoi elle a perdu.",
        DISPENSE_MARKER,
    );
    let violations = check_format("implemented", "note.md", BEFORE_ADOPTION, &content);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn a_note_dated_on_the_adoption_day_carrying_the_dispense_fails_citing_the_adoption_date() {
    let content = VALID_IMPLEMENTED.replace(
        "## Alternatives écartées\n\n**Une autre voie.** Pourquoi elle a perdu.",
        DISPENSE_MARKER,
    );
    let violations = check_format("implemented", "note.md", FORMAT_ADOPTED, &content);
    let reported = violations
        .iter()
        .find(|line| line.contains("la dispense ne vaut que"))
        .expect("the dispense is refused");
    assert!(reported.contains(FORMAT_ADOPTED), "{reported}");
}

#[test]
fn a_note_carrying_both_the_dispense_and_an_alternatives_section_fails_asking_to_drop_the_marker() {
    let content = VALID_IMPLEMENTED.replace(
        "## Alternatives écartées",
        &format!("{DISPENSE_MARKER}\n\n## Alternatives écartées"),
    );
    let violations = check_format("implemented", "note.md", BEFORE_ADOPTION, &content);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("retirer la dispense")),
        "{violations:?}"
    );
}

#[test]
fn a_marker_that_is_not_the_exact_string_is_not_a_dispense() {
    let content = VALID_IMPLEMENTED.replace(
        "## Alternatives écartées\n\n**Une autre voie.** Pourquoi elle a perdu.",
        "<!-- note-format: alternatives non consignees -->",
    );
    let violations = check_format("implemented", "note.md", BEFORE_ADOPTION, &content);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("« ## Alternatives écartées » manquante")),
        "{violations:?}"
    );
}

#[test]
fn a_note_that_is_not_valid_utf8_is_reported_without_panicking() {
    let root = scratch("invalid-utf8");
    let rel = "implemented/process/2026-08-20-illisible.md";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("the fixture has a parent"))
        .expect("the fixture directory is created");
    fs::write(&path, [0x23, 0x20, 0xff, 0xfe]).expect("the fixture file is written");
    let note = Note {
        lifecycle: "implemented".to_string(),
        class: "process".to_string(),
        rel: rel.to_string(),
        date: "2026-08-20".to_string(),
        path,
    };
    let violations = check_note_file(&note);
    let reported = violations.first().expect("the unreadable file is reported");
    assert!(reported.contains(rel), "{reported}");
    assert!(reported.contains("illisible"), "{reported}");
}

#[test]
fn every_violation_names_the_file_and_the_rule_on_a_single_line() {
    let violations = check_format(
        "implemented",
        "implemented/process/2026-08-20-x.md",
        AFTER_ADOPTION,
        "",
    );
    assert!(!violations.is_empty());
    for reported in &violations {
        assert_eq!(reported.lines().count(), 1, "{reported}");
        assert!(
            reported.starts_with("format: implemented/process/2026-08-20-x.md : "),
            "{reported}"
        );
    }
}

#[test]
fn the_gate_reads_nothing_outside_the_repository() {
    let root = notes_root();
    assert!(
        root.starts_with(repository_root()),
        "the tree is resolved inside the repository: {}",
        root.display()
    );
}
