//! Untrusted taint tracking (ARCHITECTURE 4.6, OWASP LLM01). Every tool
//! output is untrusted by default (invariant 3); when one is produced, the
//! taint is marked "recent" for a few dispatch cycles. A sensitive
//! action triggered during that window forces confirmation
//! (see `permission::resolve_permission`).
//!
//! `&self` everywhere (interior mutability): the `Registry` dispatches through `&self` and
//! runs concurrent tools that can mark the taint in parallel.

use std::sync::Mutex;

#[derive(Debug)]
struct State {
    /// Current dispatch cycle (incremented on every batch).
    cycle: u64,
    /// Last cycle where taint was produced.
    last_marked: Option<u64>,
}

/// Default freshness window (in dispatch cycles). Taint produced at
/// cycle N stays "recent" up to and including cycle N + WINDOW.
pub const DEFAULT_WINDOW: u64 = 2;

#[derive(Debug)]
pub struct TaintTracker {
    window: u64,
    state: Mutex<State>,
}

impl Default for TaintTracker {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl TaintTracker {
    pub fn new(window: u64) -> Self {
        Self {
            window,
            state: Mutex::new(State {
                cycle: 0,
                last_marked: None,
            }),
        }
    }

    /// Opens a new dispatch cycle (one tool batch). To be called once
    /// at the start of `dispatch`.
    pub fn begin_cycle(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.cycle = s.cycle.saturating_add(1);
        }
    }

    /// Marks taint at the current cycle (an untrusted output was just
    /// produced).
    pub fn mark(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.last_marked = Some(s.cycle);
        }
    }

    /// Reseed from a resumed transcript containing recent untrusted content.
    pub fn seed_recent(&self) {
        self.mark();
    }

    /// Is the taint recent (inside the window)? Used to force `Ask`.
    pub fn is_recent(&self) -> bool {
        match self.state.lock() {
            Ok(s) => match s.last_marked {
                Some(m) => s.cycle.saturating_sub(m) <= self.window,
                None => false,
            },
            // Poisoned mutex: fail-closed -> we consider the context tainted.
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_has_no_taint() {
        let t = TaintTracker::default();
        t.begin_cycle();
        assert!(!t.is_recent());
    }

    #[test]
    fn mark_makes_taint_recent_in_same_cycle() {
        let t = TaintTracker::new(2);
        t.begin_cycle();
        t.mark();
        assert!(t.is_recent());
    }

    #[test]
    fn taint_decays_after_window() {
        let t = TaintTracker::new(1); // window 1
        t.begin_cycle(); // cycle 1
        t.mark(); // marked at cycle 1
        assert!(t.is_recent());
        t.begin_cycle(); // cycle 2: 2-1=1 <= 1 -> still recent
        assert!(t.is_recent());
        t.begin_cycle(); // cycle 3: 3-1=2 > 1 -> expired
        assert!(!t.is_recent());
    }
}
