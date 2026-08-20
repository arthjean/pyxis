//! The ADR register gate: `docs/DECISIONS.md` says what it holds.
//!
//! The register announces a summary table and a per-decision format in its own
//! header, and both promises had already drifted before anything read them:
//! ADR-13 is a section its own table never lists, and four records of thirteen
//! carry no alternatives at all. An index kept by hand diverges in silence, which
//! is exactly the failure the note tree refuses to repeat, so the register is
//! held by the same kind of gate: the section set and the row set have to be
//! equal, and every documented ADR has to say what it beat.
//!
//! Numbering is compared as a set, never as a range. A retired identifier leaves
//! a hole, and a gate that turned a hole into an error would push the register
//! toward renumbering, which is the one operation the 167 `ADR-N` references
//! scattered across the repository make impossible.

use crate::DISPENSE_MARKER;
use std::fs;
use std::path::Path;

/// The register, relative to the repository root.
pub const DECISIONS_DOC: &str = "docs/DECISIONS.md";

/// The alternatives of an ADR are a bold run-in title, not a `##` heading: the
/// register nests its own sections under each decision, and the ten records that
/// already conform use this exact form.
pub const ADR_ALTERNATIVES_HEADING: &str = "**Alternatives écartées.**";

/// Read the register and report every divergence between its sections, its
/// summary table, and the format it announces.
pub fn check_decisions(repository_root: &Path) -> Vec<String> {
    let path = repository_root.join(DECISIONS_DOC);
    match fs::read_to_string(&path) {
        Ok(content) => check_decisions_document(&content),
        Err(error) => vec![format!("decisions: {DECISIONS_DOC} : illisible ({error})")],
    }
}

/// The same check over a document held in memory, so a fixture can reproduce a
/// divergence without a file on disk.
pub fn check_decisions_document(content: &str) -> Vec<String> {
    let mut sections: Vec<u32> = Vec::new();
    let mut rows: Vec<u32> = Vec::new();
    let mut missing_alternatives: Vec<String> = Vec::new();
    let mut current: Option<(u32, bool)> = None;
    let mut inside_fence = false;

    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if inside_fence {
            // A `## ADR-9` quoted inside a fenced example is an illustration, not
            // a section of the register.
            continue;
        }
        if let Some(id) = section_id(line) {
            if let Some((previous, has_alternatives)) = current
                && !has_alternatives
            {
                missing_alternatives.push(alternatives_error(previous));
            }
            sections.push(id);
            current = Some((id, false));
            continue;
        }
        if let Some((_, has_alternatives)) = current.as_mut()
            && (line.starts_with(ADR_ALTERNATIVES_HEADING) || line.trim_end() == DISPENSE_MARKER)
        {
            *has_alternatives = true;
            continue;
        }
        // Only what precedes the first decision is the summary table. A row
        // quoted inside a decision documents that decision, it does not index it.
        if sections.is_empty()
            && let Some(id) = row_id(line)
        {
            rows.push(id);
        }
    }
    if let Some((last, has_alternatives)) = current
        && !has_alternatives
    {
        missing_alternatives.push(alternatives_error(last));
    }

    let mut errors: Vec<String> = Vec::new();
    for id in &sections {
        if !rows.contains(id) {
            errors.push(format!(
                "decisions: ADR-{id} absent du tableau récapitulatif"
            ));
        }
    }
    for id in &rows {
        if !sections.contains(id) {
            errors.push(format!(
                "decisions: ADR-{id} : ligne de tableau sans section « ## ADR-{id} » correspondante"
            ));
        }
    }
    errors.extend(missing_alternatives);
    errors
}

fn alternatives_error(id: u32) -> String {
    format!(
        "decisions: ADR-{id} : « {ADR_ALTERNATIVES_HEADING} » manquante (ou la dispense, pour des alternatives non reconstructibles)"
    )
}

/// `## ADR-9 — un titre`, the identifier being what the rest of the repository
/// cites.
fn section_id(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("## ADR-")?;
    leading_number(rest)
}

/// `| ADR-9 | sujet | statut |`, the first cell and nothing else: the register
/// nests tables of its own, whose first cell never has this shape.
fn row_id(line: &str) -> Option<u32> {
    let cell = line.strip_prefix('|')?.split('|').next()?.trim();
    let rest = cell.strip_prefix("ADR-")?;
    let id = leading_number(rest)?;
    // `ADR-9 bis` is not the identifier `ADR-9`; the cell holds it and nothing
    // more, or the row is not an index row.
    (rest.trim_end() == id.to_string()).then_some(id)
}

fn leading_number(rest: &str) -> Option<u32> {
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
