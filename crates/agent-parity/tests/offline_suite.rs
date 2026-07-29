//! The offline parity recipe is executable, not decorative (US-020 AC1).
//!
//! `docs/parity/offline-suite.md` names, per contract domain, the file and the
//! test that prove it offline. A document like that rots in one commit: a test
//! gets renamed, the row keeps claiming coverage, and "parity" becomes a
//! paragraph again. So the table is PARSED here and every row is checked against
//! the repository, and the list of required domains is checked against the
//! table.
//!
//! What this does NOT do is re-run those tests: `cargo test --workspace` already
//! does, and duplicating them would double the cost of the only suite that must
//! stay cheap enough to run on every change. What it guarantees is that the
//! recipe still points at something real.
//!
//! It lives in `agent-parity` because that crate is the one that already owns
//! the baseline artifacts, and because reading repository FILES keeps its
//! invariant intact: it still imports no Pyxis crate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Every contract domain US-020 AC1 requires the offline suite to cover.
const REQUIRED_DOMAINS: &[&str] = &[
    "catalogue",
    "function-custom-wire",
    "code-mode",
    "terminal",
    "multi-agent-v2",
    "reprise",
    "app-server",
    "mcp",
    "erreurs",
];

const RECIPE: &str = "docs/parity/offline-suite.md";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root exists")
}

#[derive(Debug)]
struct Row {
    domain: String,
    file: String,
    test: String,
}

/// Rows of the single coverage table: the first markdown table whose header
/// starts with `Domaine`.
fn rows(document: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut inside = false;
    for line in document.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            inside = false;
            continue;
        }
        let cells: Vec<&str> = line
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().trim_matches('`').trim())
            .collect();
        if cells.first() == Some(&"Domaine") {
            inside = true;
            continue;
        }
        // Separator row of a markdown table.
        if cells.iter().all(|cell| cell.chars().all(|c| c == '-')) {
            continue;
        }
        if !inside || cells.len() < 4 {
            continue;
        }
        rows.push(Row {
            domain: cells[0].to_string(),
            file: cells[1].to_string(),
            test: cells[2].to_string(),
        });
    }
    rows
}

fn recipe() -> String {
    let path = repository_root().join(RECIPE);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()))
}

/// AC1: every domain the criterion names has at least one offline proof.
#[test]
fn every_required_contract_domain_is_covered() {
    let rows = rows(&recipe());
    assert!(!rows.is_empty(), "the coverage table was not parsed");
    for domain in REQUIRED_DOMAINS {
        assert!(
            rows.iter().any(|row| row.domain == *domain),
            "{RECIPE} declares no offline proof for `{domain}`"
        );
    }
    // A row for a domain nobody requires is a claim without a criterion behind
    // it: it is either a new criterion or a stale row, and both deserve a look.
    for row in &rows {
        assert!(
            REQUIRED_DOMAINS.contains(&row.domain.as_str()),
            "{RECIPE} claims a domain `{}` that US-020 AC1 does not name",
            row.domain
        );
    }
}

/// AC1: each declared proof still exists, under that name, in that file. This is
/// the check that catches a rename, which is how a coverage document usually
/// starts lying.
#[test]
fn every_declared_proof_exists_where_it_is_declared() {
    let root = repository_root();
    for row in rows(&recipe()) {
        let path = root.join(&row.file);
        let body = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{RECIPE} points at {}, which is unreadable: {error}",
                row.file
            )
        });
        assert!(
            body.contains(&format!("fn {}(", row.test)),
            "{} declares no test `{}` ({RECIPE}, domain {})",
            row.file,
            row.test,
            row.domain
        );
    }
}

/// The live recipe is named and its tri-state verdict is documented, so a
/// `skipped` can never be read as a parity success (AC3).
#[test]
fn the_live_recipe_is_documented_as_opt_in_and_tri_state() {
    let document = recipe();
    assert!(document.contains("PYXIS_LIVE_PARITY"), "{RECIPE}");
    assert!(document.contains("live-verdict.json"), "{RECIPE}");
    for status in ["skipped", "passed", "external_error"] {
        assert!(
            document.contains(status),
            "{RECIPE} does not document the `{status}` verdict"
        );
    }
    let live = repository_root().join("crates/agent-cli/tests/live_parity_sol.rs");
    assert!(live.is_file(), "{} is missing", live.display());
}
