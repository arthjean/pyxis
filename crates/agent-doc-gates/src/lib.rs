//! Structural and format checks over the repository's own decision records.
//!
//! `docs/notes/` holds one file per recorded decision, and its path carries two
//! axes: `{lifecycle}/{class}/yyyy-mm-dd-topic.md`. Encoding the lifecycle in the
//! directory is what makes a declared status that contradicts its location
//! mechanically impossible, but it only holds if something reads the tree. That
//! is this crate: one walker returning `(notes, errors)`, and one format check
//! per note, both consumed by the integration tests so a badly recorded decision
//! fails `cargo test --workspace` the way a broken test does.
//!
//! Moving a note between two lifecycles kills every link that pointed at its old
//! path, so [`links`] pays that cost in the same crate: a relative link of `docs/`
//! or of the repository root that no longer resolves fails the same suite.
//!
//! The tree does not replace `docs/DECISIONS.md`, it borders it, so [`decisions`]
//! holds the register to the same standard: an ADR its own summary table never
//! lists, or a decision that never says what it beat, fails the suite too.
//!
//! The same standard reaches the two descriptions the repository makes of its
//! own gates: [`gates`] compares the `justfile` against `.github/workflows/ci.yml`
//! so an aggregate that no longer runs what the CI runs fails the suite rather
//! than quietly promising it.
//!
//! No mechanical rule exists here without its written counterpart: the rules of
//! the tree are written in `docs/notes/README.md`, those of the register in the
//! header of `docs/DECISIONS.md`, which announces its summary table and the
//! per-decision format alike. A violation is one line naming the offending path
//! and the rule, and a run reports all of them: stopping at the first turns one
//! misplaced file into several round trips.

mod decisions;
mod gates;
mod links;

pub use decisions::{
    ADR_ALTERNATIVES_HEADING, DECISIONS_DOC, check_decisions, check_decisions_document,
};
pub use gates::{
    AGGREGATE_RECIPE, GATE_MARKER, Gate, JUSTFILE, WORKFLOW, check_gate_documents, check_gates,
    compare_gates, justfile_gates, workflow_gates,
};
pub use links::{DOCS_ROOT, check_links, markdown_documents, relative_links};

use std::fs;
use std::path::{Path, PathBuf};

/// The closed set of lifecycles: the first-level directories of the tree.
pub const LIFECYCLES: &[&str] = &["proposed", "implemented", "rejected"];

/// The closed set of classes: the directory nested under each lifecycle.
pub const CLASSES: &[&str] = &[
    "feature",
    "bug-fix",
    "simplification",
    "architecture",
    "process",
    "testing",
];

/// The day these format rules took effect. The dispense marker below is accepted
/// only on a note dated strictly before it, which is what keeps it from becoming
/// a way out for records written after the format was adopted.
pub const FORMAT_ADOPTED: &str = "2026-08-20";

/// The exact comment a pre-format note carries in place of its alternatives
/// section. Compared literally: a marker that can be paraphrased is a marker
/// that drifts.
pub const DISPENSE_MARKER: &str =
    "<!-- note-format: alternatives-non-consignees (note anterieure au format) -->";

/// The tree, relative to the repository root.
pub const NOTES_ROOT: &str = "docs/notes";

/// The only file allowed to sit directly at the tree root. A centralized index
/// is forbidden, so the tree root holds its specification and nothing else.
const ROOT_ALLOWED_FILE: &str = "README.md";

/// The universal first section of a note body.
pub const PROBLEM_HEADING: &str = "## Problème";

/// The mandatory alternatives section.
pub const ALTERNATIVES_HEADING: &str = "## Alternatives écartées";

/// Headings banned in `implemented/`: a shipped decision states what is, not what
/// was planned. Matched literally, because `docs/notes/README.md` names these four
/// titles and nothing else: a prefix match would refuse `## Planification`, a rule
/// no reader could find written anywhere.
pub const BANNED_IN_IMPLEMENTED: &[&str] = &[
    "## Proposition",
    "## Plan",
    "## Plan de migration",
    "## Critères d'acceptation",
];

/// One note, as discovered by the walker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The lifecycle directory holding the note.
    pub lifecycle: String,
    /// The class directory holding the note.
    pub class: String,
    /// Path relative to the tree root, always `/`-separated.
    pub rel: String,
    /// The `yyyy-mm-dd` prefix of the filename.
    pub date: String,
    /// Where the file actually is.
    pub path: PathBuf,
}

/// The repository root, derived from this crate's manifest so the verdict does
/// not depend on the working directory `cargo test` was launched from.
pub fn repository_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    manifest.canonicalize().unwrap_or(manifest)
}

/// The note tree of this repository.
pub fn notes_root() -> PathBuf {
    repository_root().join(NOTES_ROOT)
}

/// Walk the tree and return every valid note plus one error per structural
/// violation. An absent tree is not a violation: it means nothing is recorded
/// yet.
pub fn walk_notes(root: &Path) -> (Vec<Note>, Vec<String>) {
    let mut notes = Vec::new();
    let mut errors = Vec::new();

    let Some(entries) = sorted_entries(root) else {
        return (notes, errors);
    };
    for entry in entries {
        if entry.name == "INDEX.md" {
            errors.push(
                "structure: INDEX.md : index centralisé interdit, l'arbre est son propre inventaire"
                    .to_string(),
            );
        } else if entry.is_dir {
            // The lifecycle set is closed: any other directory would hold notes
            // that the walk below never sees.
            if !LIFECYCLES.contains(&entry.name.as_str()) {
                errors.push(format!(
                    "structure: {}/ n'est pas un cycle de vie connu (autorisés : {})",
                    entry.name,
                    LIFECYCLES.join(", ")
                ));
            }
        } else if entry.name != ROOT_ALLOWED_FILE {
            errors.push(format!(
                "structure: {} : seul {ROOT_ALLOWED_FILE} est autorisé à la racine de l'arbre",
                entry.name
            ));
        }
    }

    for lifecycle in LIFECYCLES {
        let mut relatives = Vec::new();
        collect_files(&root.join(lifecycle), lifecycle, &mut relatives);
        for rel in relatives {
            if !rel.ends_with(".md") {
                errors.push(format!(
                    "structure: {rel} : l'arbre ne contient que des fichiers .md"
                ));
                continue;
            }
            let segments: Vec<&str> = rel.split('/').collect();
            if segments.len() != 3 {
                errors.push(format!(
                    "structure: {rel} : attendu {{cycle}}/{{classe}}/fichier.md (profondeur observée : {})",
                    segments.len()
                ));
                continue;
            }
            let (Some(class), Some(base)) = (segments.get(1).copied(), segments.get(2).copied())
            else {
                continue;
            };
            if !CLASSES.contains(&class) {
                errors.push(format!(
                    "structure: {rel} : classe « {class} » inconnue (autorisées : {})",
                    CLASSES.join(", ")
                ));
                continue;
            }
            if !is_dated_filename(base) {
                errors.push(format!(
                    "structure: {rel} : le nom doit être aaaa-mm-jj-sujet.md"
                ));
                continue;
            }
            notes.push(Note {
                lifecycle: (*lifecycle).to_string(),
                class: class.to_string(),
                date: base.get(..10).unwrap_or_default().to_string(),
                path: root.join(&rel),
                rel,
            });
        }
    }

    (notes, errors)
}

/// Read a note and check its format. A file that is not valid UTF-8 is a
/// reported violation, never a panic.
pub fn check_note_file(note: &Note) -> Vec<String> {
    match fs::read(&note.path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(content) => check_format(&note.lifecycle, &note.rel, &note.date, &content),
            Err(_) => vec![format!("format: {} : illisible (UTF-8 invalide)", note.rel)],
        },
        Err(error) => vec![format!("format: {} : illisible ({error})", note.rel)],
    }
}

/// Check one note's header block, status line, section skeleton, and
/// alternatives against the lifecycle its directory declares.
///
/// `rel` only names the file in the messages, so a document that is not on disk
/// yet, such as a skeleton lifted out of the specification, can be checked the
/// same way a real note is.
pub fn check_format(lifecycle: &str, rel: &str, date: &str, content: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let at = |rule: &str| format!("format: {rel} : {rule}");
    let lines: Vec<&str> = content.lines().collect();
    let line = |index: usize| lines.get(index).copied().unwrap_or_default();

    if !line(0)
        .strip_prefix("# Note: ")
        .is_some_and(|title| !title.trim().is_empty())
    {
        errors.push(at("la ligne 1 doit être « # Note: <titre> »"));
    }
    if !line(1).is_empty() {
        errors.push(at("la ligne 2 doit être vide"));
    }
    if let Some(rule) = status_violation(lifecycle, line(2)) {
        errors.push(at(&rule));
    }
    if !line(3).is_empty() {
        errors.push(at("la ligne 4 doit être vide"));
    }

    // Format tokens inside a fenced block are an example, not this document's
    // structure. The specification itself carries three such skeletons.
    let mut inside_fence = false;
    let mut prose: Vec<&str> = Vec::new();
    for candidate in &lines {
        if candidate.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if !inside_fence {
            prose.push(candidate);
        }
    }

    if prose
        .iter()
        .filter(|candidate| candidate.starts_with("Statut:"))
        .count()
        > 1
    {
        errors.push(at("la ligne de statut doit être unique"));
    }

    let headings: Vec<&str> = prose
        .iter()
        .filter(|candidate| candidate.starts_with("## "))
        .map(|candidate| candidate.trim_end())
        .collect();
    match headings.first() {
        Some(&first) if first == PROBLEM_HEADING => {}
        Some(&first) => errors.push(at(&format!(
            "la première section doit être « {PROBLEM_HEADING} » (trouvé « {first} »)"
        ))),
        None => errors.push(at(&format!(
            "la première section doit être « {PROBLEM_HEADING} » (aucune section trouvée)"
        ))),
    }
    for required in required_headings(lifecycle) {
        if !headings.contains(required) {
            errors.push(at(&format!("section « {required} » manquante")));
        }
    }
    if lifecycle == "implemented" {
        for banned in BANNED_IN_IMPLEMENTED {
            if headings.contains(banned) {
                errors.push(at(&format!(
                    "« {banned} » est un titre de proposition, interdit dans implemented/"
                )));
            }
        }
    }

    let has_alternatives = headings.contains(&ALTERNATIVES_HEADING);
    let has_dispense = prose
        .iter()
        .any(|candidate| candidate.trim_end() == DISPENSE_MARKER);
    if has_alternatives && has_dispense {
        errors.push(at(&format!(
            "retirer la dispense, la note porte déjà « {ALTERNATIVES_HEADING} »"
        )));
    }
    if !has_alternatives && !has_dispense {
        errors.push(at(&format!(
            "section « {ALTERNATIVES_HEADING} » manquante (ou la dispense, réservée aux notes antérieures au {FORMAT_ADOPTED})"
        )));
    }
    if has_dispense && date >= FORMAT_ADOPTED {
        errors.push(at(&format!(
            "la dispense ne vaut que pour les notes antérieures au {FORMAT_ADOPTED}"
        )));
    }

    errors
}

/// The `##` headings a lifecycle requires beyond the universal `## Problème`.
pub fn required_headings(lifecycle: &str) -> &'static [&'static str] {
    match lifecycle {
        "proposed" => &["## Proposition", "## Critères d'acceptation", "## Risques"],
        "implemented" => &["## Décision", "## Conséquences"],
        "rejected" => &["## Proposition"],
        _ => &[],
    }
}

/// The status value repeats the directory name literally, so the cross-check is
/// an equality and needs no correspondence table. Rejection is the one status
/// carrying content, because its reason is what a reader comes for.
fn status_violation(lifecycle: &str, status: &str) -> Option<String> {
    if lifecycle == "rejected" {
        return match status.strip_prefix("Statut: rejected") {
            Some(rest) => match rest.strip_prefix(" - ") {
                Some(reason) if !reason.trim().is_empty() => None,
                _ => Some(
                    "un statut rejected doit porter sa raison en une ligne (« Statut: rejected - <raison> »)"
                        .to_string(),
                ),
            },
            None => Some(format!(
                "statut incompatible avec le répertoire rejected/ (attendu « Statut: rejected - <raison> », trouvé « {status} »)"
            )),
        };
    }
    let expected = format!("Statut: {lifecycle}");
    if status == expected {
        return None;
    }
    Some(format!(
        "statut incompatible avec le répertoire {lifecycle}/ (attendu « {expected} », trouvé « {status} »)"
    ))
}

/// `yyyy-mm-dd-topic.md`, the shortest of which is fifteen bytes.
fn is_dated_filename(base: &str) -> bool {
    let bytes = base.as_bytes();
    if bytes.len() < 15 || !base.ends_with(".md") {
        return false;
    }
    [0, 1, 2, 3, 5, 6, 8, 9]
        .iter()
        .all(|index| bytes.get(*index).is_some_and(u8::is_ascii_digit))
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'-')
}

/// One directory entry, read once and sorted so the report is deterministic.
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

fn sorted_entries(dir: &Path) -> Option<Vec<Entry>> {
    let mut entries: Vec<Entry> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: path.is_dir(),
                path,
            }
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Some(entries)
}

/// Collect every file under `dir`, at any depth, as a path relative to the tree
/// root. Going deeper than the two axes is a violation, so the walk has to see
/// what sits there before it can report it.
fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Some(entries) = sorted_entries(dir) else {
        return;
    };
    for entry in entries {
        let rel = format!("{prefix}/{}", entry.name);
        if entry.is_dir {
            collect_files(&entry.path, &rel, out);
        } else {
            out.push(rel);
        }
    }
}
