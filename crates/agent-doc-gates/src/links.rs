//! The link gate: every relative Markdown link of this repository resolves.
//!
//! Encoding the lifecycle of a note in its directory means a note changes status
//! by changing path, and every link that pointed at the old path dies silently.
//! That cost is the price of the tree, and this module is how it is paid: a link
//! that no longer resolves fails `cargo test --workspace` instead of rotting
//! until a reader trips on it.
//!
//! Only the relative is resolved. An `https://` target is skipped without being
//! touched, so the gate reads the disk and nothing else, and a target that climbs
//! out of the repository is a violation rather than a lookup somewhere else on the
//! machine. Anchors are dropped before resolution: proving that `#un-titre` exists
//! would need a full heading parser for a marginal gain.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// The documentation tree, relative to the repository root.
pub const DOCS_ROOT: &str = "docs";

/// Every Markdown document the gate reads: the repository's own root files plus
/// the whole documentation tree. The root is in scope because that is where the
/// links into `docs/notes/` start, `AGENTS.md` being the one file a fresh agent
/// reads before touching anything.
pub fn markdown_documents(repository_root: &Path) -> Vec<PathBuf> {
    let mut documents = Vec::new();
    if let Ok(entries) = fs::read_dir(repository_root) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() && is_markdown(&path) {
                documents.push(path);
            }
        }
    }
    collect_markdown(&repository_root.join(DOCS_ROOT), &mut documents);
    documents.sort();
    documents
}

/// Resolve every relative Markdown link of those documents against the disk and
/// return one error per dead target, each naming the source file, the line, and
/// the target.
pub fn check_links(repository_root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for document in markdown_documents(repository_root) {
        let source = relative_display(repository_root, &document);
        let Ok(content) = fs::read_to_string(&document) else {
            errors.push(format!("lien: {source} : illisible"));
            continue;
        };
        let Some(parent) = document.parent() else {
            continue;
        };
        for (line, target) in relative_links(&content) {
            let resolved = lexical_join(parent, &target);
            if !resolved.starts_with(repository_root) {
                errors.push(format!(
                    "lien: {source}:{line} : {target} sort du dépôt, un lien ne quitte pas l'arbre"
                ));
                continue;
            }
            if !resolved.exists() {
                errors.push(format!("lien: {source}:{line} : {target} est introuvable"));
            }
        }
    }
    errors
}

/// The relative link targets of one document as `(line number, target)`, anchors
/// already dropped. Fenced blocks are skipped: a document quoting a link in an
/// example is showing a shape, not pointing at a file.
pub fn relative_links(content: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut inside_fence = false;
    for (index, line) in content.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if inside_fence {
            continue;
        }
        for destination in inline_destinations(line) {
            if let Some(target) = relative_target(&destination) {
                found.push((index + 1, target));
            }
        }
    }
    found
}

/// The `(...)` destination of every inline link on a line. Parentheses are
/// counted rather than searched for, a destination being allowed to carry a
/// balanced pair of its own.
fn inline_destinations(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut destinations = Vec::new();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes.get(index) != Some(&b']') || bytes.get(index + 1) != Some(&b'(') {
            index += 1;
            continue;
        }
        let start = index + 2;
        let mut depth = 1usize;
        let mut end = start;
        while end < bytes.len() {
            match bytes.get(end) {
                Some(&b'(') => depth += 1,
                Some(&b')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        if depth != 0 {
            // The link is cut by the end of the line: nothing further to read.
            return destinations;
        }
        if let Some(destination) = line.get(start..end) {
            destinations.push(destination.to_string());
        }
        index = end + 1;
    }
    destinations
}

/// The file part of a destination, or `None` when the destination is not
/// something the disk can answer for.
fn relative_target(destination: &str) -> Option<String> {
    // `(cible "titre")`: the title is decoration, the first token is the target.
    let target = destination.split_whitespace().next().unwrap_or_default();
    let target = target.trim_start_matches('<').trim_end_matches('>');
    if target.is_empty()
        || target.starts_with('#')
        || target.starts_with("//")
        || has_scheme(target)
    {
        return None;
    }
    let file = target.split('#').next().unwrap_or(target);
    if file.is_empty() {
        return None;
    }
    Some(file.to_string())
}

/// `https:`, `mailto:` and their kind, per RFC 3986: a letter followed by letters,
/// digits, `+`, `-` or `.`, then a colon.
fn has_scheme(target: &str) -> bool {
    let mut characters = target.chars();
    if !characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
    {
        return false;
    }
    for character in characters {
        match character {
            ':' => return true,
            _ if character.is_ascii_alphanumeric() => {}
            '+' | '-' | '.' => {}
            _ => return false,
        }
    }
    false
}

/// Join a target to its source directory without touching the disk, so `..` is
/// resolved as text and no symlink is followed out of the repository.
fn lexical_join(parent: &Path, target: &str) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in parent.join(target).components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "md")
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(&path, out);
        } else if is_markdown(&path) {
            out.push(path);
        }
    }
}

/// The path a reader can act on: relative to the repository root, `/`-separated.
fn relative_display(repository_root: &Path, path: &Path) -> String {
    path.strip_prefix(repository_root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
