//! The model-experience gate: what each crate sends to the model, written down.
//!
//! The most expensive constraint of this repository lived in six code comments
//! and in no document: the prefix sent on every request is ordered `tools`,
//! `system`, `messages`, so rewording one tool `description()` throws away the
//! cache of every open session. A pull request doing exactly that read like a
//! style change, and nothing let its reviewer know otherwise.
//!
//! The answer is not one more central document. It is a section per crate, next
//! to the code it describes, and this gate, which refuses absence. Three shapes
//! close the set: a structured section for a crate whose text reaches the model,
//! a bounded short form for a crate that only changes what the model will see,
//! and a named omission for a crate nothing of which reaches it. The
//! classification is exhaustive and confronted with the disk in both directions,
//! so an unclassified crate and a stale entry both fail.
//!
//! What the gate proves is presence, order, density and anchoring. What it never
//! proves is truth: a section that is formally conforming and materially wrong
//! passes, and [`CONTRACT_DOC`] says so rather than hiding it. The one thing that
//! breaks mechanically when the code moves is a catalog anchor, which dies when a
//! tool is renamed; that is why the first field must carry a concrete literal
//! instead of a paraphrase.

use std::fs;
use std::path::Path;

use crate::crate_graph::{CRATES_ROOT, crate_directories};
use crate::links::inline_destinations;

/// The normative document. Every failure cites it: a reader who trips on the
/// gate is told where the rule is written, not only that one exists.
pub const CONTRACT_DOC: &str = "docs/model-experience.md";

/// The single heading opening the section, in a README of `crates/`.
pub const SECTION_HEADING: &str = "## Model Experience";

/// The three fields of a structured surface, in their fixed order. The set is
/// closed: an unknown H4 under a surface fails rather than being ignored.
pub const FIELDS: &[&str] = &[
    "#### What the model sees",
    "#### Token effect",
    "#### KV Cache effect",
];

/// The generated catalog whose anchors this gate resolves.
pub const TOOL_CATALOG_DOC: &str = "docs/tool-catalog.md";

/// The opening of a short form declaring no direct surface.
pub const NONE_OPENING: &str = "None, as ";

/// The opening of a short form declaring an indirect one.
pub const INDIRECT_OPENING: &str = "Indirectly, through ";

/// What makes a surface title a system-prompt surface, matched lowercase. Such a
/// surface owes a quoted block: the text sent to the model is cited, never
/// described.
pub const SYSTEM_PROMPT_MARKER: &str = "system prompt";

/// The fence a system-prompt surface opens its quotation with.
pub const MARKDOWN_FENCE: &str = "```markdown";

/// The prefix every violation of this gate carries, so a failing run reads like
/// the seven gates that came before it.
const PREFIX: &str = "expérience du modèle:";

/// The shape a crate owes, one of three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A README section with one H3 per surface and the three fields under each.
    Structured,
    /// A bounded section opening on [`NONE_OPENING`].
    ShortNone,
    /// A bounded section opening on [`INDIRECT_OPENING`].
    ShortIndirect,
    /// No file at all: the justification lives in the table.
    Omitted,
}

impl Shape {
    /// How a message names the shape.
    pub fn label(self) -> &'static str {
        match self {
            Self::Structured => "structurée",
            Self::ShortNone | Self::ShortIndirect => "forme courte",
            Self::Omitted => "omission justifiée",
        }
    }

    /// The opening the classification sentence must carry, for a short form.
    pub fn opening(self) -> Option<&'static str> {
        match self {
            Self::ShortNone => Some(NONE_OPENING),
            Self::ShortIndirect => Some(INDIRECT_OPENING),
            Self::Structured | Self::Omitted => None,
        }
    }

    /// True of the two short forms, whichever opening they carry.
    pub fn is_short(self) -> bool {
        matches!(self, Self::ShortNone | Self::ShortIndirect)
    }
}

/// One crate and the shape it owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    /// The directory under `crates/`, which is also the package name.
    pub name: &'static str,
    /// The shape, one of three.
    pub shape: Shape,
    /// Why, in plain words. Mandatory for the two non-structured shapes, where
    /// it is the only place the reason is readable; empty for a structured
    /// crate, whose reason is the section itself.
    pub justification: &'static str,
}

/// The three shapes, named in the failure a missing classification produces.
const SHAPES: &str = "structurée, forme courte ou omission justifiée";

/// The sixteen crates of the workspace, ordered by name so an insertion reads in
/// the diff at its place. This is the table [`CONTRACT_DOC`] describes, and it is
/// a Rust constant rather than a parsed markdown table because this crate's
/// `[dependencies]` is empty on purpose: a malformed entry here is a compilation
/// error, and a parser would be one more hand-written thing to keep right.
pub const CLASSIFICATION: &[Classified] = &[
    Classified {
        name: "agent-app-server",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-auth",
        shape: Shape::Omitted,
        justification: "Le crate parle au keyring de l'OS et au point d'autorisation OAuth, jamais au modèle : aucun de ses textes n'entre dans une requête, et un échec d'authentification arrête le tour avant qu'un préfixe soit assemblé.",
    },
    Classified {
        name: "agent-cli",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-code-mode",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-code-mode-v8",
        shape: Shape::ShortIndirect,
        justification: "Le moteur n'écrit aucun texte de son cru : ce que la cellule produit remonte par les helpers du protocole, et seule une exception de l'isolat atteint le modèle, à travers le résultat d'outil que rend agent-tools.",
    },
    Classified {
        name: "agent-core",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-doc-gates",
        shape: Shape::Omitted,
        justification: "Le crate lit les documents du dépôt et rend un verdict à cargo test : il n'entre dans le graphe d'aucun binaire livré, donc aucun de ses octets ne peut atteindre une requête modèle.",
    },
    Classified {
        name: "agent-mcp",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-parity",
        shape: Shape::Omitted,
        justification: "Le crate lit le clone Codex épinglé et rend des matrices de contrat : il ne s'exécute qu'en vérification, hors de toute session, et rien de ce qu'il produit n'est injecté dans un tour.",
    },
    Classified {
        name: "agent-provider",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-runtime",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-sandbox",
        shape: Shape::ShortIndirect,
        justification: "Le confinement ne rédige rien, mais le corps de refus 403 du proxy réseau traverse la sortie de l'outil d'exécution qui l'a reçu et arrive tel quel dans le transcript.",
    },
    Classified {
        name: "agent-session",
        shape: Shape::ShortIndirect,
        justification: "La persistance rejoue ce qui a déjà été envoyé et n'ajoute aucun texte de son cru : elle décide quels messages une reprise remet dans le transcript, pas ce qu'ils disent.",
    },
    Classified {
        name: "agent-tokenizer",
        shape: Shape::ShortIndirect,
        justification: "Le compteur n'écrit aucun texte, mais il décide quand la compaction se déclenche lorsque le fournisseur omet son usage, donc ce que le modèle verra au tour suivant.",
    },
    Classified {
        name: "agent-tools",
        shape: Shape::Structured,
        justification: "",
    },
    Classified {
        name: "agent-tui",
        shape: Shape::Omitted,
        justification: "Le sens de la flèche est l'argument : le crate rend des AgentEvent vers l'humain et ne produit jamais de message entrant, donc rien de ce qu'il écrit ne peut remonter dans une requête.",
    },
];

/// The classification entry of `name`, if it has one.
pub fn classification_of(name: &str) -> Option<&'static Classified> {
    CLASSIFICATION.iter().find(|entry| entry.name == name)
}

/// One line of a document, with the knowledge of whether it sits inside a fenced
/// block. A `## Model Experience` quoted in an example is showing a shape, not
/// opening a section, and the whole gate reads lines through this lens.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    number: usize,
    text: &'a str,
    /// True inside a fenced block AND on the fence delimiters themselves.
    fenced: bool,
}

fn scan(content: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut inside = false;
    for (index, raw) in content.lines().enumerate() {
        let delimiter = raw.trim_start().starts_with("```");
        lines.push(Line {
            number: index + 1,
            text: raw,
            fenced: inside || delimiter,
        });
        if delimiter {
            inside = !inside;
        }
    }
    lines
}

/// The heading level of a line, `None` when it is not a heading. A `#` not
/// followed by a space is not a title, which is what keeps `#!/bin/sh` and an
/// issue reference out.
fn heading_level(text: &str) -> Option<usize> {
    let trimmed = text.trim();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    trimmed
        .chars()
        .nth(hashes)
        .filter(|c| *c == ' ')
        .map(|_| hashes)
}

/// The title of a heading, hashes and surrounding spaces removed.
fn heading_title(text: &str) -> &str {
    text.trim().trim_start_matches('#').trim()
}

/// How many paragraphs of prose a field body holds. Fenced blocks and headings
/// are quoted matter and structure, not prose: a field carrying a `##### ` title
/// and a quoted block still holds exactly one paragraph.
fn paragraph_count(body: &[Line<'_>]) -> usize {
    let mut count = 0;
    let mut running = false;
    for line in body {
        if line.fenced || line.text.trim().is_empty() || heading_level(line.text).is_some() {
            running = false;
            continue;
        }
        if !running {
            count += 1;
            running = true;
        }
    }
    count
}

/// The prose of a field body, joined into one string. Used for the one sentence
/// a short form owes, which may wrap across lines.
fn paragraph_text(body: &[Line<'_>]) -> String {
    let mut words = Vec::new();
    for line in body {
        if line.fenced || heading_level(line.text).is_some() {
            continue;
        }
        words.extend(line.text.split_whitespace());
    }
    words.join(" ")
}

/// The lines of the `## Model Experience` section, or the reason there is none.
enum Section<'a> {
    /// The section, its lines, and the line its heading sits on.
    Found(Vec<Line<'a>>),
    /// No heading at all.
    Absent,
    /// More than one heading: the crate declares its surfaces twice.
    Duplicated(usize),
}

fn section<'a>(lines: &[Line<'a>]) -> Section<'a> {
    let openings: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.fenced && line.text.trim() == SECTION_HEADING)
        .map(|(index, _)| index)
        .collect();
    if openings.len() > 1 {
        return Section::Duplicated(openings.len());
    }
    let Some(&start) = openings.first() else {
        return Section::Absent;
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| !line.fenced && heading_level(line.text).is_some_and(|level| level <= 2))
        .map_or(lines.len(), |(index, _)| index);
    Section::Found(lines.get(start + 1..end).unwrap_or_default().to_vec())
}

/// Split a run of lines at every heading of `level`, returning the blocks each
/// opening heading introduces, the heading line included.
fn blocks_at<'a>(lines: &[Line<'a>], level: usize) -> Vec<Vec<Line<'a>>> {
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.fenced && heading_level(line.text) == Some(level))
        .map(|(index, _)| index)
        .collect();
    let mut blocks = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        blocks.push(lines.get(start..end).unwrap_or_default().to_vec());
    }
    blocks
}

/// The body of a field: everything after its H4 up to the next heading of level
/// four or shallower. A `##### ` title does NOT close a field, which is what lets
/// a system-prompt surface carry its titled quotation inside `What the model
/// sees`.
fn field_body<'a>(block: &[Line<'a>], heading_index: usize) -> Vec<Line<'a>> {
    let end = block
        .iter()
        .enumerate()
        .skip(heading_index + 1)
        .find(|(_, line)| !line.fenced && heading_level(line.text).is_some_and(|level| level <= 4))
        .map_or(block.len(), |(index, _)| index);
    block
        .get(heading_index + 1..end)
        .unwrap_or_default()
        .to_vec()
}

/// True when the field body carries a concrete literal: inline code, a nested
/// block, or an anchored link into the tool catalog. A paraphrase carries none of
/// the three, and a paraphrase is what goes stale without a sound.
fn is_anchored(body: &[Line<'_>]) -> bool {
    body.iter().any(|line| {
        line.fenced
            || line.text.matches('`').count() >= 2
            || line.text.contains(&format!("{TOOL_CATALOG_DOC}#"))
    })
}

/// True when the body quotes a system prompt the way the contract demands: a
/// titled `##### ` heading, then a ```markdown block after it.
fn quotes_a_prompt(body: &[Line<'_>]) -> bool {
    let Some(titled) = body.iter().position(|line| {
        !line.fenced && heading_level(line.text) == Some(5) && !heading_title(line.text).is_empty()
    }) else {
        return false;
    };
    body.iter()
        .skip(titled + 1)
        .any(|line| line.text.trim().starts_with(MARKDOWN_FENCE))
}

/// Confront the classification with the disk, in both directions, and hold every
/// non-structured entry to a readable justification. The table is an argument
/// rather than the constant read from inside, so the rules it carries can be
/// falsified on a fixture instead of being taken on trust.
pub fn check_classification(entries: &[Classified], directories: &[String]) -> Vec<String> {
    if directories.is_empty() {
        return vec![format!(
            "{PREFIX} {CRATES_ROOT}/ est vide ou illisible ; une classification vide n'est pas une classification, voir {CONTRACT_DOC}"
        )];
    }
    let mut violations = Vec::new();
    for directory in directories {
        if !entries.iter().any(|entry| entry.name == *directory) {
            violations.push(format!(
                "{PREFIX} `{directory}` n'est pas classé : {SHAPES}, voir {CONTRACT_DOC}"
            ));
        }
    }
    for entry in entries {
        if !directories.iter().any(|name| name == entry.name) {
            violations.push(format!(
                "{PREFIX} l'entrée `{}` ne correspond à aucun crate ; une exception dont la raison a disparu est un défaut, voir {CONTRACT_DOC}",
                entry.name
            ));
            continue;
        }
        if entry.shape == Shape::Structured {
            continue;
        }
        if entry.justification.split_whitespace().count() < 2 {
            violations.push(format!(
                "{PREFIX} `{}` est classé {} sans motif lisible ; une omission sans raison écrite est un oubli déguisé, voir {CONTRACT_DOC}",
                entry.name,
                entry.shape.label()
            ));
        }
    }
    violations
}

/// The anchors `docs/tool-catalog.md` publishes, derived from its `### \`name\``
/// headings. A catalog rendering no section is an error rather than a licence to
/// accept every link, on the model of the catalog's own harvest guard.
///
/// The slug rule is deliberately narrow: lowercase, backticks removed, spaces
/// hyphenated, and nothing outside `[a-z0-9_-]`. A tool named outside that set
/// fails the gate instead of producing a silently wrong link.
pub fn catalog_fragments(content: &str) -> Result<Vec<String>, Vec<String>> {
    let mut fragments = Vec::new();
    let mut errors = Vec::new();
    for line in scan(content) {
        if line.fenced || heading_level(line.text) != Some(3) {
            continue;
        }
        let title = heading_title(line.text);
        let fragment = title
            .replace('`', "")
            .trim()
            .to_lowercase()
            .replace(' ', "-");
        if fragment.is_empty()
            || !fragment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            errors.push(format!(
                "{PREFIX} {TOOL_CATALOG_DOC}:{} : le titre « {title} » sort du jeu attendu `[a-z0-9_-]`, la règle de fragment ne le couvre pas, voir {CONTRACT_DOC}",
                line.number
            ));
            continue;
        }
        fragments.push(fragment);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    if fragments.is_empty() {
        return Err(vec![format!(
            "{PREFIX} {TOOL_CATALOG_DOC} ne rend aucune section, récolte impossible, voir {CONTRACT_DOC}"
        )]);
    }
    fragments.sort();
    fragments.dedup();
    Ok(fragments)
}

/// Every link of `content` aiming at a catalog fragment that does not exist. A
/// link carrying no fragment is accepted here and answered by the link gate: the
/// two responsibilities do not overlap.
pub fn check_catalog_anchors(name: &str, content: &str, fragments: &[String]) -> Vec<String> {
    let mut violations = Vec::new();
    for line in scan(content) {
        if line.fenced {
            continue;
        }
        for destination in inline_destinations(line.text) {
            let target = destination.split_whitespace().next().unwrap_or_default();
            let target = target.trim_start_matches('<').trim_end_matches('>');
            let Some((path, fragment)) = target.split_once('#') else {
                continue;
            };
            if fragment.is_empty() || !path.ends_with(TOOL_CATALOG_DOC) {
                continue;
            }
            if !fragments.iter().any(|known| known == fragment) {
                violations.push(format!(
                    "{PREFIX} `{name}:{}` : `#{fragment}` n'existe pas parmi les {} sections du catalogue, voir {CONTRACT_DOC}",
                    line.number,
                    fragments.len()
                ));
            }
        }
    }
    violations
}

/// Hold one README to the shape its entry declares. `content` is `None` when the
/// file is absent, which is a violation for a structured or short crate and the
/// nominal state for an omitted one.
pub fn check_readme(
    entry: &Classified,
    content: Option<&str>,
    fragments: &[String],
) -> Vec<String> {
    let name = entry.name;
    let rel = format!("{CRATES_ROOT}/{name}/README.md");
    let Some(content) = content else {
        if entry.shape == Shape::Omitted {
            return Vec::new();
        }
        return vec![format!(
            "{PREFIX} `{rel}` est attendu et absent pour un crate classé {}, voir {CONTRACT_DOC}",
            entry.shape.label()
        )];
    };
    let lines = scan(content);
    let section = match section(&lines) {
        Section::Found(section) => section,
        Section::Duplicated(count) => {
            return vec![format!(
                "{PREFIX} `{name}` porte {count} sections `{SECTION_HEADING}` ; une seule dit ce que le modèle reçoit, voir {CONTRACT_DOC}"
            )];
        }
        Section::Absent => {
            if entry.shape == Shape::Omitted {
                return Vec::new();
            }
            return vec![format!(
                "{PREFIX} `{rel}` ne porte aucune section `{SECTION_HEADING}` alors que le crate est classé {}, voir {CONTRACT_DOC}",
                entry.shape.label()
            )];
        }
    };
    if entry.shape == Shape::Omitted {
        return vec![format!(
            "{PREFIX} `{name}` est classé en omission et porte pourtant une section `{SECTION_HEADING}` ; les deux déclarations se contredisent et la table tranche, voir {CONTRACT_DOC}"
        )];
    }
    let mut violations = if entry.shape.is_short() {
        check_short_section(entry, &section)
    } else {
        check_structured_section(name, &section)
    };
    violations.extend(check_catalog_anchors(name, content, fragments));
    violations
}

/// The structured form: one H3 per surface, three ordered fields under each, one
/// paragraph per field, a literal under the first, and a quoted block when the
/// surface names the system prompt.
fn check_structured_section(name: &str, section: &[Line<'_>]) -> Vec<String> {
    let surfaces = blocks_at(section, 3);
    if surfaces.is_empty() {
        return vec![format!(
            "{PREFIX} `{name}` : la section ne porte aucun H3 ; une section sans surface ne dit rien, voir {CONTRACT_DOC}"
        )];
    }
    let mut violations = Vec::new();
    for block in &surfaces {
        let Some(heading) = block.first() else {
            continue;
        };
        let surface = heading_title(heading.text);
        let found: Vec<(usize, &str)> = block
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.fenced && heading_level(line.text) == Some(4))
            .map(|(index, line)| (index, line.text.trim()))
            .collect();
        for (_, title) in &found {
            if !FIELDS.contains(title) {
                violations.push(format!(
                    "{PREFIX} `{name}` / `{surface}` : champ inconnu « {title} » ; l'ensemble des champs est fermé, voir {CONTRACT_DOC}"
                ));
            }
        }
        let ordered: Vec<&str> = found
            .iter()
            .map(|(_, title)| *title)
            .filter(|title| FIELDS.contains(title))
            .collect();
        if ordered != FIELDS {
            violations.push(format!(
                "{PREFIX} `{name}` / `{surface}` : ordre attendu `What the model sees`, `Token effect`, `KV Cache effect`, lu « {} », voir {CONTRACT_DOC}",
                ordered.join(" | ")
            ));
        }
        for (index, title) in &found {
            if !FIELDS.contains(title) {
                continue;
            }
            let body = field_body(block, *index);
            let paragraphs = paragraph_count(&body);
            if paragraphs != 1 {
                violations.push(format!(
                    "{PREFIX} `{name}` / `{surface}` / `{title}` : {} paragraphe(s), exactement un est attendu, voir {CONTRACT_DOC}",
                    paragraphs
                ));
            }
            if *title != FIELDS.first().copied().unwrap_or_default() {
                continue;
            }
            if !is_anchored(&body) {
                violations.push(format!(
                    "{PREFIX} `{name}` / `{surface}` : ni code inline, ni bloc imbriqué, ni lien ancré vers {TOOL_CATALOG_DOC} ; une paraphrase n'ancre rien, voir {CONTRACT_DOC}"
                ));
            }
            if surface.to_lowercase().contains(SYSTEM_PROMPT_MARKER) && !quotes_a_prompt(&body) {
                violations.push(format!(
                    "{PREFIX} `{name}` / `{surface}` : le texte système se cite, il ne se décrit pas ; un H5 titré suivi d'un bloc {MARKDOWN_FENCE} est attendu, voir {CONTRACT_DOC}"
                ));
            }
        }
    }
    violations
}

/// The short form: a classification sentence, then the cache field, and nothing
/// else. The form is closed, otherwise it becomes a degraded structured one.
fn check_short_section(entry: &Classified, section: &[Line<'_>]) -> Vec<String> {
    let name = entry.name;
    let mut violations = Vec::new();
    if section
        .iter()
        .any(|line| !line.fenced && heading_level(line.text) == Some(3))
    {
        violations.push(format!(
            "{PREFIX} `{name}` est en forme courte, un H3 y est interdit ; la forme est fermée, voir {CONTRACT_DOC}"
        ));
    }
    let cache_field = FIELDS.get(2).copied().unwrap_or_default();
    let fields: Vec<(usize, &str)> = section
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.fenced && heading_level(line.text) == Some(4))
        .map(|(index, line)| (index, line.text.trim()))
        .collect();
    for (_, title) in &fields {
        if *title != cache_field {
            violations.push(format!(
                "{PREFIX} `{name}` est en forme courte, `{title}` y est interdit ; seul `{cache_field}` est admis, voir {CONTRACT_DOC}"
            ));
        }
    }
    let opening = section.get(..fields.first().map_or(section.len(), |(index, _)| *index));
    let opening = opening.unwrap_or_default();
    let paragraphs = paragraph_count(opening);
    if paragraphs != 1 {
        violations.push(format!(
            "{PREFIX} `{name}` : {paragraphs} paragraphe(s) de classification, exactement une phrase est attendue, voir {CONTRACT_DOC}"
        ));
    } else {
        violations.extend(check_opening(entry, &paragraph_text(opening)));
    }
    match fields.iter().find(|(_, title)| *title == cache_field) {
        Some((index, _)) => {
            let body = field_body(section, *index);
            let paragraphs = paragraph_count(&body);
            if paragraphs != 1 {
                violations.push(format!(
                    "{PREFIX} `{name}` / `{cache_field}` : {paragraphs} paragraphe(s), exactement un est attendu, voir {CONTRACT_DOC}"
                ));
            }
        }
        None => violations.push(format!(
            "{PREFIX} `{name}` est en forme courte sans `{cache_field}` ; la déclaration d'absence d'effet demande le même raisonnement que la déclaration d'effet, voir {CONTRACT_DOC}"
        )),
    }
    violations
}

/// The classification sentence against the shape the table declares.
fn check_opening(entry: &Classified, sentence: &str) -> Vec<String> {
    let name = entry.name;
    let Some(expected) = entry.shape.opening() else {
        return Vec::new();
    };
    let other = if expected == NONE_OPENING {
        INDIRECT_OPENING
    } else {
        NONE_OPENING
    };
    let mut violations = Vec::new();
    if sentence.starts_with(other) {
        violations.push(format!(
            "{PREFIX} `{name}` : la table déclare « {expected} » et le README ouvre sur « {other} » ; les deux se contredisent, voir {CONTRACT_DOC}"
        ));
    } else if !sentence.starts_with(expected) {
        violations.push(format!(
            "{PREFIX} `{name}` : la phrase de classification ouvre sur une amorce inconnue ; les deux admises sont « {NONE_OPENING} » et « {INDIRECT_OPENING} », voir {CONTRACT_DOC}"
        ));
    }
    if !sentence.trim_end().ends_with('.') {
        violations.push(format!(
            "{PREFIX} `{name}` : la phrase de classification ne se termine pas par un point, voir {CONTRACT_DOC}"
        ));
    }
    violations
}

/// The whole gate over this repository: the classification against the disk, the
/// catalog harvest, then every README the table names.
pub fn check_model_experience(repository_root: &Path) -> Vec<String> {
    let directories = crate_directories(&repository_root.join(CRATES_ROOT));
    let mut violations = check_classification(CLASSIFICATION, &directories);
    if directories.is_empty() {
        return violations;
    }
    let catalog = repository_root.join(TOOL_CATALOG_DOC);
    let fragments = match fs::read_to_string(&catalog) {
        Ok(content) => match catalog_fragments(&content) {
            Ok(fragments) => fragments,
            Err(errors) => {
                violations.extend(errors);
                return violations;
            }
        },
        Err(error) => {
            violations.push(format!(
                "{PREFIX} {TOOL_CATALOG_DOC} : illisible ({error}), voir {CONTRACT_DOC}"
            ));
            return violations;
        }
    };
    for entry in CLASSIFICATION {
        if !directories.iter().any(|name| name == entry.name) {
            continue;
        }
        let path = repository_root
            .join(CRATES_ROOT)
            .join(entry.name)
            .join("README.md");
        let content = fs::read_to_string(&path).ok();
        violations.extend(check_readme(entry, content.as_deref(), &fragments));
    }
    violations
}
