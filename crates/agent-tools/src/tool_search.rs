//! Deferred tool loading and the `tool_search` tool (ARCHITECTURE 4.5), ported
//! from Codex (`codex-rs/core/src/tools/handlers/tool_search.rs`).
//!
//! The problem is a cost the user never sees: every exposed tool carries its
//! description and its JSON schema into EVERY request of the conversation. A
//! handful of native tools is negligible. Three MCP servers is not: the schemas
//! alone routinely outweigh the transcript, on every turn, forever.
//!
//! So past [`DEFER_THRESHOLD`] exposed tools, the deferrable ones (in practice
//! the MCP tools: the native surface is small, stable, and needed on most turns)
//! leave the request and are replaced by ONE `tool_search`. The model searches,
//! the matches are revealed, and from the next turn on they are exposed
//! normally. The threshold is the architecture's: below it, deferring buys
//! nothing and costs a round trip.
//!
//! Two deliberate differences from the baseline:
//!
//! 1. **Deferral is ours, not the provider's.** Codex sets `defer_loading` on
//!    the wire and lets the backend hold the specs. Doing it here means it works
//!    on every provider, including those with no such field, and that the wire
//!    stays a projection of what the registry decided rather than a second
//!    source of truth. The `defer_loading` flag still travels when a spec sets
//!    it; nothing here contradicts it.
//! 2. **Scoring is lexical, not BM25.** A ranking over a few hundred short
//!    documents does not need an index: the discriminating signal is whether the
//!    query's words appear in the tool's name and description, and a name match
//!    outweighs a description match. A BM25 dependency for that would be weight
//!    without an answer to "which query does it get right that this one does
//!    not?".
//!
//! What deferral never does is change what may be CALLED. A deferred tool is
//! still registered, still dispatchable, and still goes through the same
//! pipeline: hiding a spec is a prompt-cost decision, never a permission one.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use agent_core::provider::ToolSpec;
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Exposed-tool count past which deferral starts (ARCHITECTURE 4.5). Below it,
/// the prompt cost is negligible and deferring only adds latency.
pub const DEFER_THRESHOLD: usize = 15;
/// Matches returned by one search when the model does not say.
pub const DEFAULT_LIMIT: usize = 8;
/// Hard cap, whatever the model asks for: a search that returns forty schemas
/// has undone the deferral it was called to make cheap.
pub const MAX_LIMIT: usize = 20;
/// Name of the tool. Shared with the registry, which exposes it only when
/// something is actually deferred.
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// One deferred tool, as the search sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredEntry {
    pub name: String,
    pub description: String,
    /// Kept whole: revealing a tool has to hand the model the same schema it
    /// would have read in the request, not a summary of it.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Default)]
struct State {
    entries: Vec<DeferredEntry>,
    revealed: BTreeSet<String>,
}

/// Shared handle over what is deferred and what has been revealed.
///
/// The registry publishes into it (it owns the tools) and `tool_search` reads
/// and reveals through it. `&self` everywhere: the registry publishes at a turn
/// boundary while a call may be reading.
#[derive(Debug, Clone, Default)]
pub struct DeferredTools {
    inner: Arc<RwLock<State>>,
}

impl DeferredTools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes the deferrable set. Called by the registry whenever the exposed
    /// set moves. Revealed names are KEPT across a republish: a tool the model
    /// already found must not disappear because a server reconnected.
    pub fn publish(&self, entries: Vec<DeferredEntry>) {
        let mut state = self.write();
        // A revealed name whose tool is gone is dropped: keeping it would expose
        // a spec for something no longer registered.
        let live: BTreeSet<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        state.revealed.retain(|name| live.contains(name.as_str()));
        state.entries = entries;
    }

    /// Has this tool been revealed by a search? A tool that is not deferrable at
    /// all is not "revealed": the registry never asks about those.
    pub fn is_revealed(&self, name: &str) -> bool {
        self.read().revealed.contains(name)
    }

    /// Names currently hidden from the model, for `/tools` and for tests.
    pub fn hidden(&self) -> Vec<String> {
        let state = self.read();
        state
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .filter(|name| !state.revealed.contains(name))
            .collect()
    }

    fn reveal(&self, names: impl IntoIterator<Item = String>) {
        let mut state = self.write();
        state.revealed.extend(names);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Ranks the deferred entries against `query`. Already-revealed tools stay
    /// in the results: a model that searches twice for the same thing must find
    /// it twice, not get an empty answer that reads as "it does not exist".
    fn search(&self, query: &str, limit: usize) -> Vec<DeferredEntry> {
        let terms = terms_of(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let state = self.read();
        let mut scored: Vec<(u32, usize, &DeferredEntry)> = state
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let score = score(entry, &terms);
                (score > 0).then_some((score, index, entry))
            })
            .collect();
        // Score first, then declaration order: a stable ranking is what makes
        // two identical searches cacheable and testable.
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| entry.clone())
            .collect()
    }
}

/// Weight of a term found in the tool NAME. A name match is a much stronger
/// signal than a description match: descriptions share generic vocabulary
/// ("file", "list", "the server"), names do not.
const NAME_WEIGHT: u32 = 8;
const DESCRIPTION_WEIGHT: u32 = 1;

fn score(entry: &DeferredEntry, terms: &[String]) -> u32 {
    let name = entry.name.to_ascii_lowercase();
    let description = entry.description.to_ascii_lowercase();
    terms
        .iter()
        .map(|term| {
            let mut score = 0;
            if name.contains(term.as_str()) {
                score += NAME_WEIGHT;
            }
            if description.contains(term.as_str()) {
                score += DESCRIPTION_WEIGHT;
            }
            score
        })
        .sum()
}

/// Splits a query into searchable terms. Single characters are dropped: they
/// match everything and rank nothing.
fn terms_of(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.chars().count() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSearchInput {
    /// What the tool should do, in words. Matched against tool names and
    /// descriptions.
    pub query: String,
    /// How many matches to return. Defaults to [`DEFAULT_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Finds tools that were left out of the request, and reveals them.
pub struct ToolSearch {
    deferred: DeferredTools,
}

impl ToolSearch {
    pub fn new(deferred: DeferredTools) -> Self {
        Self { deferred }
    }
}

#[async_trait]
impl Tool for ToolSearch {
    type Input = ToolSearchInput;

    fn name(&self) -> &str {
        TOOL_SEARCH_NAME
    }
    fn description(&self) -> String {
        "Find tools that are available but not listed in this request. Many \
         tools (typically those from MCP servers) are kept out of the prompt \
         until needed. Search by what you want to do (\"create a GitHub issue\", \
         \"read a Postgres table\"); the matches become callable from your next \
         turn."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What the tool should do, in words."
                },
                "limit": {
                    "type": ["integer", "null"],
                    "description": format!(
                        "How many matches to return (default {DEFAULT_LIMIT}, max {MAX_LIMIT})."
                    )
                }
            },
            "required": ["query", "limit"],
            "additionalProperties": false
        })
    }
    /// It reads a catalog and marks names as revealed. Nothing is executed and
    /// no perimeter moves: revealing a spec grants no permission, because every
    /// revealed tool still goes through the whole pipeline when called.
    fn is_read_only(&self) -> bool {
        true
    }
    /// Two searches in one batch are the normal case when a model is looking for
    /// two different capabilities.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// The names and descriptions come from MCP servers, which are third
    /// parties: what this returns is server-authored text and is tainted like
    /// any other tool output.
    fn returns_untrusted(&self) -> bool {
        true
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        SEARCH_GUIDELINES
    }

    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.query.trim().is_empty() {
            return Err(ValidationError::new("query is empty"));
        }
        if input.limit == Some(0) {
            return Err(ValidationError::new("limit must be greater than zero"));
        }
        Ok(())
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let matches = self.deferred.search(&input.query, limit);
        if matches.is_empty() {
            // Saying "nothing matched" is not the same as saying "nothing
            // exists": a model told the second stops looking, and the tool it
            // needed was two words away.
            let hidden = self.deferred.hidden().len();
            return Ok(ToolOutput::text(format!(
                "No tool matched \"{}\". {hidden} tool(s) are currently hidden; \
                 try other words, or work with the tools already listed.",
                input.query.trim()
            )));
        }
        self.deferred
            .reveal(matches.iter().map(|entry| entry.name.clone()));
        let rendered: Vec<String> = matches
            .iter()
            .map(|entry| {
                format!(
                    "- {}: {}\n  input schema: {}",
                    entry.name, entry.description, entry.input_schema
                )
            })
            .collect();
        Ok(ToolOutput::text(format!(
            "{} tool(s) matched and are now available. They appear in your tool \
             list from the next turn on; call them then, not now.\n{}",
            matches.len(),
            rendered.join("\n")
        )))
    }
}

const SEARCH_GUIDELINES: &[&str] = &[
    "tool_search: some tools are hidden from the request to save context. If \
     the capability you need is not in your tool list, search for it before \
     concluding it does not exist. A match becomes callable on the NEXT turn.",
];

/// Applies the deferral to a set of specs: past the threshold, deferrable tools
/// that have not been revealed leave the request.
///
/// Kept as a free function so the decision is testable without a registry, and
/// so the registry keeps one call site instead of a filtering branch inside its
/// snapshot path.
pub fn apply_deferral(
    specs: Vec<ToolSpec>,
    deferrable: &BTreeSet<String>,
    deferred: &DeferredTools,
) -> Vec<ToolSpec> {
    // The threshold counts the WHOLE exposed set, deferrable or not: what costs
    // context is the request, and the native tools are part of it.
    if specs.len() <= DEFER_THRESHOLD || deferrable.is_empty() {
        return specs;
    }
    specs
        .into_iter()
        .filter(|spec| !deferrable.contains(&spec.name) || deferred.is_revealed(&spec.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, description: &str) -> DeferredEntry {
        DeferredEntry {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn catalog() -> DeferredTools {
        let deferred = DeferredTools::new();
        deferred.publish(vec![
            entry(
                "mcp__github__create_issue",
                "Open an issue on a repository.",
            ),
            entry(
                "mcp__github__list_pulls",
                "List the pull requests of a repository.",
            ),
            entry(
                "mcp__pg__query",
                "Run a read-only SQL query on the database.",
            ),
        ]);
        deferred
    }

    #[test]
    fn a_name_match_outranks_a_description_match() {
        let deferred = catalog();
        let found = deferred.search("query the database", 5);
        assert_eq!(
            found.first().map(|entry| entry.name.as_str()),
            Some("mcp__pg__query"),
            "the tool whose NAME carries the term must come first: {found:?}"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_returns_nothing_rather_than_everything() {
        assert!(catalog().search("kubernetes", 5).is_empty());
        // Single characters match everything and rank nothing.
        assert!(catalog().search("a", 5).is_empty());
    }

    #[tokio::test]
    async fn a_search_reveals_its_matches_and_says_they_are_callable_next_turn() {
        let deferred = catalog();
        assert_eq!(deferred.hidden().len(), 3);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = ToolSearch::new(deferred.clone())
            .call(
                ToolSearchInput {
                    query: "issue".to_string(),
                    limit: None,
                },
                &ctx,
            )
            .await
            .expect("a search must succeed");
        assert!(deferred.is_revealed("mcp__github__create_issue"));
        assert_eq!(deferred.hidden().len(), 2);
        assert!(out.content.contains("next turn"), "{}", out.content);
        // The schema travels whole: the model must read what it would have read
        // in the request.
        assert!(out.content.contains("input schema"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_result_says_how_many_tools_remain_hidden() {
        let deferred = catalog();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = ToolSearch::new(deferred)
            .call(
                ToolSearchInput {
                    query: "kubernetes".to_string(),
                    limit: None,
                },
                &ctx,
            )
            .await
            .expect("an empty search is a result");
        assert!(
            out.content.contains("3 tool(s) are currently hidden"),
            "{}",
            out.content
        );
        assert!(!out.is_error, "finding nothing is not a failure");
    }

    #[test]
    fn republishing_forgets_a_revealed_tool_that_no_longer_exists() {
        let deferred = catalog();
        deferred.reveal(["mcp__pg__query".to_string()]);
        assert!(deferred.is_revealed("mcp__pg__query"));
        deferred.publish(vec![entry("mcp__github__create_issue", "Open an issue.")]);
        assert!(
            !deferred.is_revealed("mcp__pg__query"),
            "a revealed name whose tool is gone must not keep exposing a spec"
        );
    }

    fn specs(count: usize, deferrable_from: usize) -> (Vec<ToolSpec>, BTreeSet<String>) {
        let mut specs = Vec::new();
        let mut deferrable = BTreeSet::new();
        for index in 0..count {
            let name = format!("tool_{index}");
            if index >= deferrable_from {
                deferrable.insert(name.clone());
            }
            specs.push(ToolSpec::function(
                name,
                "d",
                serde_json::json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            ));
        }
        (specs, deferrable)
    }

    #[test]
    fn below_the_threshold_nothing_is_deferred() {
        let (all, deferrable) = specs(DEFER_THRESHOLD, 2);
        let kept = apply_deferral(all.clone(), &deferrable, &DeferredTools::new());
        assert_eq!(kept.len(), all.len());
    }

    #[test]
    fn above_the_threshold_only_the_deferrable_and_unrevealed_leave() {
        let (all, deferrable) = specs(DEFER_THRESHOLD + 5, DEFER_THRESHOLD);
        let deferred = DeferredTools::new();
        deferred.publish(
            deferrable
                .iter()
                .map(|name| entry(name, "deferred"))
                .collect(),
        );
        deferred.reveal([format!("tool_{DEFER_THRESHOLD}")]);
        let kept = apply_deferral(all, &deferrable, &deferred);
        let names: BTreeSet<String> = kept.into_iter().map(|spec| spec.name).collect();
        assert!(names.contains("tool_0"), "a native tool never leaves");
        assert!(
            names.contains(&format!("tool_{DEFER_THRESHOLD}")),
            "a revealed tool is exposed again"
        );
        assert!(
            !names.contains(&format!("tool_{}", DEFER_THRESHOLD + 1)),
            "an unrevealed deferrable tool leaves the request"
        );
    }
}
