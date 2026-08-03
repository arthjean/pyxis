//! Session-scoped record of the tool calls that were attempted, ported in
//! intent from Codex (`codex-rs/core/src/tools/executed_tool_calls.rs`).
//!
//! The transcript already carries every call the MODEL made. What it does not
//! carry is what the pipeline did around them: how long a call took, whether it
//! was refused before running, whether it was a nested Code Mode call the model
//! never sees, and how often a tool is retried. Those questions come up on every
//! real diagnosis ("why is this session slow?", "which tool keeps failing?") and
//! today they are answerable only by re-reading a trace file, which exists only
//! when `PYXIS_LOG` was set BEFORE the run.
//!
//! Three bounds keep it from becoming a second transcript:
//!
//! 1. **No arguments and no output.** A tool input can carry a file body, a
//!    command line, or a token. Recording it here would duplicate the redaction
//!    problem the observability policy already solved once, in a structure with
//!    no redaction of its own. Only names, statuses and durations travel.
//! 2. **A ring, not a list.** [`MAX_ENTRIES`] calls are kept; older ones fall
//!    off. A long session must not grow memory through its own bookkeeping.
//! 3. **Aggregates survive the ring.** The per-tool counters keep counting after
//!    an entry is dropped, so "this tool failed 40 times" stays true even when
//!    the individual calls are gone.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_core::message::ToolErrorKind;
use agent_core::tools::{ModelToolResult, ToolResultStatus};

/// Individual calls retained. Past this the ring drops the oldest; the
/// aggregates below keep counting.
pub const MAX_ENTRIES: usize = 256;

/// One attempted call, with nothing in it that could carry user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchEntry {
    pub tool: String,
    pub status: ToolResultStatus,
    pub error_kind: Option<ToolErrorKind>,
    pub duration: Option<Duration>,
    /// Did the output enter the context as untrusted (OWASP LLM01)? Recorded
    /// because "which calls tainted this turn" is the first question asked when
    /// a confirmation appears out of nowhere.
    pub untrusted: bool,
}

/// Per-tool totals. Kept separately from the ring so they stay true for the
/// whole session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolTotals {
    pub calls: u64,
    pub failures: u64,
    /// Summed wall time of the calls that reported one.
    pub total_duration: Duration,
}

#[derive(Debug, Default)]
struct State {
    entries: std::collections::VecDeque<DispatchEntry>,
    totals: BTreeMap<String, ToolTotals>,
    /// Calls dropped from the ring, so a reader can say the list is partial
    /// instead of implying it is complete.
    dropped: u64,
}

/// Shared handle on the record. Cloned into the registry and into whatever
/// displays it; `&self` everywhere, because concurrent tools finish in parallel.
#[derive(Debug, Clone, Default)]
pub struct DispatchLog {
    inner: Arc<RwLock<State>>,
}

impl DispatchLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one finished call. Called by the registry for every outcome,
    /// including refusals and unknown tools: a call that never ran is exactly
    /// the one a diagnosis is looking for.
    pub fn record(&self, tool: &str, outcome: &ModelToolResult) {
        let entry = DispatchEntry {
            tool: tool.to_string(),
            status: outcome.status,
            error_kind: outcome.error_kind,
            duration: outcome.duration_ms.map(Duration::from_millis),
            untrusted: outcome.untrusted,
        };
        let mut state = self.write();
        let totals = state.totals.entry(entry.tool.clone()).or_default();
        totals.calls = totals.calls.saturating_add(1);
        if outcome.is_error {
            totals.failures = totals.failures.saturating_add(1);
        }
        if let Some(duration) = entry.duration {
            totals.total_duration = totals.total_duration.saturating_add(duration);
        }
        if state.entries.len() >= MAX_ENTRIES {
            state.entries.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.entries.push_back(entry);
    }

    /// The retained calls, oldest first.
    pub fn entries(&self) -> Vec<DispatchEntry> {
        self.read().entries.iter().cloned().collect()
    }

    /// Per-tool totals, by tool name.
    pub fn totals(&self) -> BTreeMap<String, ToolTotals> {
        self.read().totals.clone()
    }

    /// Calls that fell off the ring. Non-zero means [`Self::entries`] is a tail,
    /// not the whole session.
    pub fn dropped(&self) -> u64 {
        self.read().dropped
    }

    /// One-line summary per tool, most-called first, for `/status`.
    pub fn summary(&self) -> Vec<String> {
        let totals = self.totals();
        let mut rows: Vec<(&String, &ToolTotals)> = totals.iter().collect();
        rows.sort_by(|left, right| {
            right
                .1
                .calls
                .cmp(&left.1.calls)
                .then_with(|| left.0.cmp(right.0))
        });
        rows.into_iter()
            .map(|(tool, totals)| {
                let average = totals
                    .total_duration
                    .checked_div(u32::try_from(totals.calls).unwrap_or(u32::MAX))
                    .unwrap_or_default();
                format!(
                    "{tool}: {} call(s), {} failed, {} ms average",
                    totals.calls,
                    totals.failures,
                    average.as_millis()
                )
            })
            .collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(id: &str, is_error: bool, ms: u64) -> ModelToolResult {
        let mut outcome = ModelToolResult::new(
            id.to_string(),
            "body".to_string(),
            is_error,
            true,
            is_error.then_some(ToolErrorKind::Semantic),
        );
        outcome.duration_ms = Some(ms);
        outcome
    }

    #[test]
    fn totals_survive_the_ring() {
        let log = DispatchLog::new();
        for index in 0..(MAX_ENTRIES + 10) {
            log.record("bash", &outcome(&index.to_string(), index % 2 == 0, 10));
        }
        assert_eq!(log.entries().len(), MAX_ENTRIES, "the ring is bounded");
        assert_eq!(log.dropped(), 10, "what fell off is counted, not hidden");
        let totals = log.totals();
        let bash = totals.get("bash").expect("recorded");
        assert_eq!(
            bash.calls,
            (MAX_ENTRIES + 10) as u64,
            "an aggregate must stay true after an entry is dropped"
        );
        assert_eq!(bash.failures, ((MAX_ENTRIES + 10) as u64).div_ceil(2));
    }

    #[test]
    fn a_refusal_is_recorded_like_any_other_call() {
        // The call a diagnosis is looking for is usually the one that never ran.
        let log = DispatchLog::new();
        let refused =
            ModelToolResult::rejected("c1".to_string(), "denied", ToolErrorKind::PermissionDenied);
        log.record("write", &refused);
        let entries = log.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, ToolResultStatus::Rejected);
        assert_eq!(entries[0].error_kind, Some(ToolErrorKind::PermissionDenied));
        assert_eq!(log.totals()["write"].failures, 1);
    }

    #[test]
    fn the_summary_ranks_by_call_count() {
        let log = DispatchLog::new();
        log.record("read", &outcome("a", false, 4));
        log.record("read", &outcome("b", false, 6));
        log.record("bash", &outcome("c", true, 100));
        let summary = log.summary();
        assert!(summary[0].starts_with("read: 2 call(s)"), "{summary:?}");
        assert!(summary[0].contains("5 ms average"), "{summary:?}");
        assert!(summary[1].starts_with("bash: 1 call(s), 1 failed"), "{summary:?}");
    }

    #[test]
    fn nothing_recorded_carries_call_content() {
        // The structural guarantee: `DispatchEntry` has no field that could hold
        // an argument or an output, so no redaction is needed here and none can
        // be forgotten. This test fails to compile if a field is added.
        let log = DispatchLog::new();
        log.record("bash", &outcome("a", false, 1));
        let DispatchEntry {
            tool: _,
            status: _,
            error_kind: _,
            duration: _,
            untrusted: _,
        } = log.entries().remove(0);
    }
}
