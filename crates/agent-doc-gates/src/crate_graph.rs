//! The crate graph, derived from the sixteen manifests of the workspace.
//!
//! Two tables of this repository claimed to list every crate and neither did:
//! `README.md` named ten of sixteen, `docs/ARCHITECTURE.md` eleven, and the
//! ASCII diagram next to it drew eleven and missed an edge. Nothing signalled
//! it, because an exhaustive table written by hand goes stale one line per new
//! crate and stays green forever. This module is the answer: `crates/*/Cargo.toml`
//! is the source, a pure function renders the whole document, and an integration
//! test compares the rendered bytes to what the repository holds.
//!
//! Three properties make the verdict worth trusting:
//!
//! - The render is pure. [`render_crate_graph`] takes parsed manifests and
//!   returns a `String`; it reads nothing and writes nothing, so the same tree
//!   renders the same bytes on any machine, in any locale, from any working
//!   directory. Every order is explicit and no `HashMap` reaches it.
//! - A case the parser does not know fails. An unreadable `[dependencies]`
//!   entry, a manifest without `description`, an edge aimed at a crate that does
//!   not exist: each names its file and its line rather than rendering an empty
//!   cell, because an empty cell is what a stale document looks like.
//! - Freshness alone is not enough. A generator that forgets a crate renders a
//!   document its own comparison accepts, so [`check_crate_graph_completeness`]
//!   confronts the rendered table with the directories on disk, in both
//!   directions.
//!
//! The role of a crate is the `description` field of its own manifest, so the
//! prose that used to be duplicated in two documents now lives next to the code
//! it describes and travels with it.

use std::fs;
use std::path::Path;

/// The rendered document, relative to the repository root.
pub const CRATE_GRAPH_DOC: &str = "docs/crate-graph.md";

/// The directory holding the crates of the workspace.
pub const CRATES_ROOT: &str = "crates";

/// The environment variable that flips the freshness test into writing. Named
/// here so the header of the document, the failure message and the test all cite
/// the same string.
pub const UPDATE_VARIABLE: &str = "PYXIS_UPDATE_CATALOGS";

/// The exact command that regenerates the document. It is the head of the
/// rendered file and the remedy printed by a failing comparison: a reader who
/// finds a stale document is told what to run, not what went wrong.
pub const REGENERATE_COMMAND: &str =
    "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-doc-gates --test crate_graph";

/// The module rendering the document, cited in its header.
pub const GENERATOR: &str = "crates/agent-doc-gates/src/crate_graph.rs";

/// What a cell holds when a crate has no internal dependency. An empty cell
/// reads like a missing value; this one reads like an answer.
pub const NO_DEPENDENCY: &str = "aucune";

/// The prefix internal crates share (ADR-8), and therefore the one test telling
/// an edge of the workspace from an external dependency.
const INTERNAL_PREFIX: &str = "agent-";

/// One crate, as its manifest declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateManifest {
    /// The `name` of the `[package]` section.
    pub name: String,
    /// The `description` of the `[package]` section: the role of the crate.
    pub description: String,
    /// The `agent-*` entries of its dependency sections, sorted and deduplicated.
    pub internal_dependencies: Vec<String>,
}

/// Which kind of section the parser is walking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// `[package]`, where the name and the role are read.
    Package,
    /// `[dependencies]` and its target-conditional forms, where the edges are.
    Dependencies,
    /// Everything else, including `[dev-dependencies]` and `[build-dependencies]`.
    Other,
}

/// Parse one manifest. `rel` only names the file in the messages, so a fixture
/// that is not on disk can be parsed the way a real manifest is.
///
/// The TOML is read by hand, line by line, the way the `justfile` and the note
/// tree already are: this crate declares an empty `[dependencies]` table and a
/// parser dependency would be the first entry in it. The scope that buys is
/// closed on purpose, and anything outside it is reported instead of guessed.
pub fn parse_manifest(rel: &str, content: &str) -> Result<CrateManifest, Vec<String>> {
    let mut errors = Vec::new();
    let mut section = Section::Other;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut dependencies: Vec<String> = Vec::new();

    for (index, raw) in content.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = classify_section(line);
            continue;
        }
        match section {
            Section::Package => {
                if let Some(value) = quoted_value(line, "name") {
                    name = Some(value);
                } else if let Some(value) = quoted_value(line, "description") {
                    description = Some(value);
                }
            }
            Section::Dependencies => match dependency_key(line) {
                Some(key) if key.starts_with(INTERNAL_PREFIX) => dependencies.push(key),
                Some(_) => {}
                None => errors.push(format!(
                    "graphe: {rel}:{number} : entrée de dépendance illisible « {line} »"
                )),
            },
            Section::Other => {}
        }
    }

    let Some(name) = name else {
        errors.push(format!("graphe: {rel} : « name » absent de [package]"));
        return Err(errors);
    };
    let Some(description) = description else {
        errors.push(format!(
            "graphe: {rel} : « description » absente ; le rôle du crate vit dans son manifeste"
        ));
        return Err(errors);
    };
    if description.trim().is_empty() {
        errors.push(format!(
            "graphe: {rel} : « description » vide ; le rôle du crate vit dans son manifeste"
        ));
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    dependencies.retain(|dependency| *dependency != name);
    dependencies.sort();
    dependencies.dedup();
    Ok(CrateManifest {
        name,
        description,
        internal_dependencies: dependencies,
    })
}

/// The kind of a section header.
///
/// A target-conditional table is a dependency section too: `[target.'cfg(unix)'.dependencies]`
/// carries dependencies a build really links, and reading only the literal
/// `[dependencies]` would drop such an edge without a word. Development and
/// build tables are excluded, because neither says what a shipped binary
/// depends on.
fn classify_section(line: &str) -> Section {
    let Some(start) = line.find('[') else {
        return Section::Other;
    };
    let Some(end) = line.rfind(']') else {
        return Section::Other;
    };
    let header = line
        .get(start + 1..end)
        .unwrap_or_default()
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if header == "package" {
        return Section::Package;
    }
    let table = header.rsplit('.').next().unwrap_or(header);
    if table == "dependencies" {
        Section::Dependencies
    } else {
        Section::Other
    }
}

/// The value of `key = "..."` on one line, if that is what the line is.
fn quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let value = rest.strip_prefix('=')?.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.to_string())
}

/// The dependency name a line declares, or `None` when the line is not a shape
/// this parser accepts.
///
/// Three shapes exist in the sixteen manifests and no other is guessed at:
/// `name.workspace = true`, `name = { ... }` on a single line, and
/// `name = "version"`. A multi-line inline table would return `None` on its
/// opening line and be reported, which is the intended outcome: the entry has to
/// be readable, not merely present.
fn dependency_key(line: &str) -> Option<String> {
    let (left, right) = line.split_once('=')?;
    let key = left.trim();
    if key.is_empty() {
        return None;
    }
    let name = key.split('.').next().unwrap_or(key).trim();
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return None;
    }
    // The subkey path, when there is one, is dotted identifiers and nothing else.
    for subkey in key.split('.').skip(1) {
        let subkey = subkey.trim();
        if subkey.is_empty()
            || !subkey.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
        {
            return None;
        }
    }
    let value = right.trim();
    let readable = (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || value == "true"
        || value == "false"
        || (value.starts_with('{') && value.ends_with('}'));
    readable.then(|| name.to_string())
}

/// Render the whole document, header included, from the parsed manifests.
///
/// Pure: it reads no file, writes none, and consults no clock. The order of
/// every list is an explicit sort, so two runs on the same tree produce the same
/// bytes.
pub fn render_crate_graph(manifests: &[CrateManifest]) -> Result<String, Vec<String>> {
    let mut errors = Vec::new();
    if manifests.is_empty() {
        return Err(vec![format!(
            "graphe: {CRATES_ROOT}/ illisible ou vide ; un graphe vide n'est pas un graphe"
        )]);
    }
    let mut sorted: Vec<&CrateManifest> = manifests.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));

    let mut names: Vec<&str> = sorted
        .iter()
        .map(|manifest| manifest.name.as_str())
        .collect();
    let before = names.len();
    names.dedup();
    if names.len() != before {
        errors.push("graphe: deux manifestes déclarent le même « name »".to_string());
    }
    for manifest in &sorted {
        for dependency in &manifest.internal_dependencies {
            if !names.contains(&dependency.as_str()) {
                errors.push(format!(
                    "graphe: {} dépend de « {dependency} », qui n'est pas un crate de {CRATES_ROOT}/",
                    manifest.name
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "<!-- Généré par {GENERATOR} ; ne pas éditer à la main. -->\n"
    ));
    out.push_str(&format!("<!-- Régénérer : {REGENERATE_COMMAND} -->\n"));
    out.push('\n');
    out.push_str("# Graphe de crates\n\n");
    out.push_str(&format!(
        "Les {} crates de `{CRATES_ROOT}/` et leurs arêtes internes, dérivés de leurs manifestes.\n",
        sorted.len()
    ));
    out.push_str(
        "Le rôle d'un crate est le champ `description` de son propre `Cargo.toml` : il se corrige\n\
         là, jamais ici, et ce document est réécrit par la commande de son en-tête.\n\n",
    );
    out.push_str(
        "Une arête est une entrée `agent-*` d'une section `[dependencies]`, y compris\n\
         conditionnelle par cible. `[dev-dependencies]` et `[build-dependencies]` sont exclues :\n\
         elles ne disent pas de quoi un binaire publié dépend. Les dépendances externes ne\n\
         figurent pas ici, chacune étant argumentée dans le manifeste qui la porte. Les\n\
         dépendances qu'un crate s'interdit sont un invariant et non un fait dérivable : elles\n\
         restent écrites dans [`ARCHITECTURE.md`](ARCHITECTURE.md).\n\n",
    );

    out.push_str("```mermaid\ngraph LR\n");
    for manifest in &sorted {
        out.push_str(&format!(
            "    {}[\"{}\"]\n",
            node_identifier(&manifest.name),
            manifest.name
        ));
    }
    let mut edges: Vec<(&str, &str)> = Vec::new();
    for manifest in &sorted {
        for dependency in &manifest.internal_dependencies {
            edges.push((manifest.name.as_str(), dependency.as_str()));
        }
    }
    edges.sort_unstable();
    if !edges.is_empty() {
        out.push('\n');
    }
    for (from, to) in &edges {
        out.push_str(&format!(
            "    {} --> {}\n",
            node_identifier(from),
            node_identifier(to)
        ));
    }
    out.push_str("```\n\n");

    out.push_str("| Crate | Rôle | Dépend de |\n|---|---|---|\n");
    for manifest in &sorted {
        let depends = if manifest.internal_dependencies.is_empty() {
            NO_DEPENDENCY.to_string()
        } else {
            manifest
                .internal_dependencies
                .iter()
                .map(|dependency| format!("`{dependency}`"))
                .collect::<Vec<String>>()
                .join(", ")
        };
        out.push_str(&format!(
            "| `{}` | {} | {depends} |\n",
            manifest.name,
            escape_cell(&manifest.description)
        ));
    }
    Ok(out)
}

/// A Mermaid node identifier. The crate names carry hyphens and `-->` is the
/// edge operator, so the identifier is the name with underscores and the label
/// carries the real name.
fn node_identifier(name: &str) -> String {
    name.replace('-', "_")
}

/// A table cell holds one line and no unescaped pipe, whatever the manifest
/// wrote. A description is prose the repository controls, but a cell that
/// silently breaks its table would be a rendering bug nothing catches.
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// The crate directories on disk: every `crates/<name>/` holding a `Cargo.toml`.
/// Sorted, so the report of a violation is deterministic.
pub fn crate_directories(crates_root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(crates_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("Cargo.toml").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Parse every manifest under `crates_root`, reporting all failures at once.
pub fn collect_manifests(crates_root: &Path) -> Result<Vec<CrateManifest>, Vec<String>> {
    let directories = crate_directories(crates_root);
    if directories.is_empty() {
        return Err(vec![format!(
            "graphe: {CRATES_ROOT}/ illisible ou vide ; un graphe vide n'est pas un graphe"
        )]);
    }
    let mut manifests = Vec::new();
    let mut errors = Vec::new();
    for directory in directories {
        let rel = format!("{CRATES_ROOT}/{directory}/Cargo.toml");
        match fs::read_to_string(crates_root.join(&directory).join("Cargo.toml")) {
            Ok(content) => match parse_manifest(&rel, &content) {
                Ok(manifest) => manifests.push(manifest),
                Err(reported) => errors.extend(reported),
            },
            Err(error) => errors.push(format!("graphe: {rel} : illisible ({error})")),
        }
    }
    if errors.is_empty() {
        Ok(manifests)
    } else {
        Err(errors)
    }
}

/// The document this repository should hold, rendered from its own manifests.
pub fn crate_graph_document(repository_root: &Path) -> Result<String, Vec<String>> {
    let manifests = collect_manifests(&repository_root.join(CRATES_ROOT))?;
    render_crate_graph(&manifests)
}

/// The crate names the rendered table lists, in the order it lists them.
pub fn rendered_crates(rendered: &str) -> Vec<String> {
    rendered
        .lines()
        .filter_map(|line| line.trim().strip_prefix("| `"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_string)
        .collect()
}

/// Confront the rendered document with the directories on disk, in both
/// directions.
///
/// Freshness alone cannot catch this: a generator that skips a crate renders a
/// document identical to the one it published, and the byte comparison stays
/// green over the omission. The guard is what makes the table exhaustive rather
/// than merely reproducible.
pub fn check_crate_graph_completeness(rendered: &str, directories: &[String]) -> Vec<String> {
    if directories.is_empty() {
        return vec![format!(
            "graphe: {CRATES_ROOT}/ illisible ou vide ; un graphe vide n'est pas un graphe"
        )];
    }
    let listed = rendered_crates(rendered);
    let mut violations = Vec::new();
    for directory in directories {
        if !listed.iter().any(|name| name == directory) {
            violations.push(format!(
                "graphe: {directory} a un Cargo.toml et n'apparaît pas dans le graphe rendu"
            ));
        }
    }
    for name in &listed {
        if !directories.iter().any(|directory| directory == name) {
            violations.push(format!(
                "graphe: {name} est dans le graphe rendu et n'a pas de répertoire {CRATES_ROOT}/{name}/"
            ));
        }
    }
    violations
}

/// The completeness guard over the repository itself.
pub fn check_crate_graph(repository_root: &Path, rendered: &str) -> Vec<String> {
    check_crate_graph_completeness(
        rendered,
        &crate_directories(&repository_root.join(CRATES_ROOT)),
    )
}
