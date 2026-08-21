//! The gate over `docs/EVENT_SCHEMA.md` (US-127).
//!
//! The document is a machine contract: an integrator writes a parser from it,
//! so a type it does not list is a type that parser will not recognize. It had
//! drifted twice, in two ways a reader cannot notice. Six variants of
//! `AgentEvent` had no row at all, and the identifier prefixes of its examples
//! (`th_`, `tu_`, `ev_`) were three strings the binary never emits.
//!
//! Two independent checks close the two failures, because one alone would keep
//! accepting the other. [`check_event_types`] compares the variants declared in
//! `crates/agent-core/src/event.rs` to the rows of the published table and
//! reports the gap by name and by count, so a variant added without its row
//! fails `cargo test --workspace` rather than shipping silently.
//! [`check_examples`] holds every `json` block of the document to a frozen
//! transcript: an anchored block IS the line it names, compared byte for byte,
//! and a block no scenario can produce must say so above itself. An example
//! nobody can check is how the wrong prefixes survived a year of reading.
//!
//! The parser here is textual on purpose. This crate imports no Pyxis crate, so
//! it cannot ask `agent-core` for its own variant list; it reads the source the
//! way [`crate_graph`](crate::crate_graph) reads a manifest, tracking brace
//! depth so a struct variant's fields never pass for variants of their own.
//! That constraint is also what makes the gate honest: it proves the DOCUMENT
//! against the SOURCE, not the source against itself.

use std::fs;
use std::path::Path;

/// The document under gate.
pub const EVENT_SCHEMA_DOC: &str = "docs/EVENT_SCHEMA.md";

/// The source of truth it is compared to.
pub const EVENT_SOURCE: &str = "crates/agent-core/src/event.rs";

/// Where the variant list starts in that source.
pub const ENUM_HEADER: &str = "pub enum AgentEvent {";

/// The section holding the published table.
pub const TYPES_HEADING: &str = "## Types d'événements";

/// The one documented row that is not an `AgentEvent` variant. It comes from
/// the runtime, the document says so on the line right under the table, and the
/// count would be off by one forever if the gate did not know it by name.
pub const NON_VARIANT_ROWS: &[&str] = &["thread_store_failed"];

/// Opens an example that IS a line of a frozen transcript.
pub const TRANSCRIPT_ANCHOR: &str = "<!-- transcription:";

/// Opens an example no frozen scenario can produce, followed by the reason.
pub const UNFROZEN_ANCHOR: &str = "<!-- hors transcription:";

/// The fence the two markers above must precede.
pub const JSON_FENCE: &str = "```json";

/// The variants of `AgentEvent`, in declaration order, under the names serde
/// puts on the wire (`#[serde(rename_all = "snake_case")]`).
///
/// Fails rather than returning an empty list when the enum cannot be found or
/// never closes: a gate that silently compares zero variants to twenty-four
/// rows would report the document as wrong for the one reason it is not.
pub fn variant_names(source: &str) -> Result<Vec<String>, String> {
    let start = source
        .find(ENUM_HEADER)
        .ok_or_else(|| format!("{EVENT_SOURCE} ne contient pas `{ENUM_HEADER}`"))?;
    let body = &source[start + ENUM_HEADER.len()..];
    let mut names = Vec::new();
    let mut depth: i32 = 0;
    for line in body.lines() {
        let trimmed = line.trim();
        // A comment is never counted, in depth or in names: a doc comment holding
        // an unbalanced brace would otherwise hide every variant after it.
        if trimmed.starts_with("//") {
            continue;
        }
        if depth == 0 {
            if trimmed == "}" {
                return Ok(names);
            }
            if let Some(name) = leading_identifier(trimmed) {
                names.push(snake_case(&name));
            }
        }
        depth += balance(trimmed);
    }
    Err(format!(
        "l'énumération `AgentEvent` de {EVENT_SOURCE} ne se referme pas"
    ))
}

/// The identifier a variant line opens with, when it opens with one.
fn leading_identifier(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let first = chars.next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let name: String = line
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    Some(name)
}

/// What serde renames a variant to.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (rank, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() && rank > 0 {
            out.push('_');
        }
        out.push(character.to_ascii_lowercase());
    }
    out
}

/// Nesting a line opens or closes.
fn balance(line: &str) -> i32 {
    line.chars().fold(0, |depth, character| match character {
        '{' | '(' | '[' => depth + 1,
        '}' | ')' | ']' => depth - 1,
        _ => depth,
    })
}

/// The `type` column of the table under [`TYPES_HEADING`], in published order.
pub fn documented_types(document: &str) -> Result<Vec<String>, String> {
    let start = document.find(TYPES_HEADING).ok_or_else(|| {
        format!("{EVENT_SCHEMA_DOC} ne contient pas la section `{TYPES_HEADING}`")
    })?;
    let mut types = Vec::new();
    let mut inside = false;
    for line in document[start..].lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            if inside {
                break;
            }
            continue;
        }
        // The separator row is what tells the header row apart from the data,
        // without the gate having to know what the header says.
        if trimmed
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' '))
        {
            inside = true;
            continue;
        }
        if inside {
            let cell = trimmed
                .trim_start_matches('|')
                .split('|')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('`')
                .to_string();
            types.push(cell);
        }
    }
    if types.is_empty() {
        return Err(format!(
            "la table de `{TYPES_HEADING}` dans {EVENT_SCHEMA_DOC} est vide"
        ));
    }
    Ok(types)
}

/// The comparison the epic exists for: every variant has a row, every row that
/// is not declared as coming from elsewhere has a variant, and the counts are
/// named so the reader sees the size of the gap before reading the list.
pub fn check_event_types(variants: &[String], documented: &[String]) -> Vec<String> {
    let mut violations = Vec::new();
    let expected: Vec<&String> = documented
        .iter()
        .filter(|entry| !NON_VARIANT_ROWS.contains(&entry.as_str()))
        .collect();
    if variants.len() != expected.len() {
        violations.push(format!(
            "{EVENT_SCHEMA_DOC} a dérivé de {EVENT_SOURCE} : {} variantes, {} types documentés",
            variants.len(),
            expected.len()
        ));
    }
    for variant in variants {
        if !expected.contains(&variant) {
            violations.push(format!(
                "la variante `{variant}` d'`AgentEvent` n'a pas de ligne dans {EVENT_SCHEMA_DOC}"
            ));
        }
    }
    for entry in &expected {
        if !variants.contains(entry) {
            violations.push(format!(
                "le type `{entry}` documenté dans {EVENT_SCHEMA_DOC} n'est pas une variante d'`AgentEvent` ; s'il vient d'ailleurs, il entre dans NON_VARIANT_ROWS"
            ));
        }
    }
    violations
}

/// One `json` block of the document, with what precedes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    /// 1-based rank of the opening fence in the document.
    pub line: usize,
    /// The marker line above the fence, when there is one.
    pub anchor: Option<String>,
    /// The lines between the two fences.
    pub body: Vec<String>,
}

/// Every `json` block, with the marker line that precedes it.
pub fn examples(document: &str) -> Vec<Example> {
    let lines: Vec<&str> = document.lines().collect();
    let mut found = Vec::new();
    let mut rank = 0;
    while rank < lines.len() {
        if lines[rank].trim() != JSON_FENCE {
            rank += 1;
            continue;
        }
        let anchor = rank
            .checked_sub(1)
            .map(|above| lines[above].trim().to_string())
            .filter(|above| !above.is_empty());
        let mut body = Vec::new();
        let mut cursor = rank + 1;
        while cursor < lines.len() && lines[cursor].trim() != "```" {
            body.push(lines[cursor].to_string());
            cursor += 1;
        }
        found.push(Example {
            line: rank + 1,
            anchor,
            body,
        });
        rank = cursor + 1;
    }
    found
}

/// Every example is either a transcript line, byte for byte, or declares that
/// no scenario produces it. Nothing in between: an unmarked block is how a
/// hand-written example passes for observed output.
pub fn check_examples(root: &Path, document: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for example in examples(document) {
        let position = format!("{EVENT_SCHEMA_DOC}:{}", example.line);
        let Some(anchor) = example.anchor else {
            violations.push(format!(
                "{position} : un bloc `json` sans marqueur ; il porte `{TRANSCRIPT_ANCHOR} <chemin>:<rang> -->` ou `{UNFROZEN_ANCHOR} <raison> -->`"
            ));
            continue;
        };
        if anchor.starts_with(UNFROZEN_ANCHOR) {
            let reason = anchor
                .trim_start_matches(UNFROZEN_ANCHOR)
                .trim_end_matches("-->")
                .trim();
            if reason.is_empty() {
                violations.push(format!(
                    "{position} : `{UNFROZEN_ANCHOR}` sans raison ; dire pourquoi aucun scénario gelé n'émet ce type"
                ));
            }
            continue;
        }
        if !anchor.starts_with(TRANSCRIPT_ANCHOR) {
            violations.push(format!(
                "{position} : le bloc `json` est précédé de `{anchor}` au lieu d'un marqueur ; il porte `{TRANSCRIPT_ANCHOR} <chemin>:<rang> -->` ou `{UNFROZEN_ANCHOR} <raison> -->`"
            ));
            continue;
        }
        let target = anchor
            .trim_start_matches(TRANSCRIPT_ANCHOR)
            .trim_end_matches("-->")
            .trim();
        let Some((path, rank)) = target.rsplit_once(':') else {
            violations.push(format!(
                "{position} : `{target}` n'est pas de la forme `<chemin>:<rang>`"
            ));
            continue;
        };
        let Ok(rank) = rank.parse::<usize>() else {
            violations.push(format!("{position} : `{rank}` n'est pas un rang de ligne"));
            continue;
        };
        let Ok(frozen) = fs::read_to_string(root.join(path)) else {
            violations.push(format!("{position} : {path} est illisible"));
            continue;
        };
        let Some(expected) = frozen.lines().nth(rank.saturating_sub(1)) else {
            violations.push(format!("{position} : {path} n'a pas de ligne {rank}"));
            continue;
        };
        if example.body.len() != 1 {
            violations.push(format!(
                "{position} : un exemple ancré est une ligne de transcription, or le bloc en compte {}",
                example.body.len()
            ));
            continue;
        }
        let found = example.body.first().map(String::as_str).unwrap_or_default();
        if found != expected {
            violations.push(format!(
                "{position} : l'exemple ne correspond plus à {path}:{rank}\n  gelé :   {expected}\n  publié : {found}"
            ));
        }
    }
    violations
}

/// The whole gate over this repository's own document.
pub fn check_event_schema(root: &Path) -> Vec<String> {
    let document = match fs::read_to_string(root.join(EVENT_SCHEMA_DOC)) {
        Ok(document) => document,
        Err(err) => return vec![format!("{EVENT_SCHEMA_DOC} est illisible : {err}")],
    };
    let source = match fs::read_to_string(root.join(EVENT_SOURCE)) {
        Ok(source) => source,
        Err(err) => return vec![format!("{EVENT_SOURCE} est illisible : {err}")],
    };
    let variants = match variant_names(&source) {
        Ok(variants) => variants,
        Err(err) => return vec![err],
    };
    let documented = match documented_types(&document) {
        Ok(documented) => documented,
        Err(err) => return vec![err],
    };
    let mut violations = check_event_types(&variants, &documented);
    violations.extend(check_examples(root, &document));
    violations
}
