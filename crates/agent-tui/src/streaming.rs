//! Stable-prefix plus mutable-tail streaming for the parity transcript path.
//!
//! The controller owns the raw assistant markdown source. It exposes a stable
//! prefix that can be treated as committed for rendering purposes, and a mutable
//! tail that remains live until a newline-safe boundary or finalization.
//!
//! On top of that it owns the display pacing. Providers deliver deltas in
//! bursts: a whole paragraph can arrive in one chunk and the next one 400 ms
//! later. Handing those bursts straight to the screen makes the transcript jump.
//! Instead, rendered lines of the stable prefix are queued, and a commit tick
//! releases them at a steady rate ([`ChunkingMode::Smooth`]) unless the queue
//! falls behind, in which case it drains the backlog at once
//! ([`ChunkingMode::CatchUp`]) so display lag converges instead of accumulating.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::text::Line;

/// Baseline interval between two released lines in smooth mode.
///
/// Fast enough to read as motion rather than as steps, slow enough that a whole
/// paragraph does not land in one frame.
pub const COMMIT_TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Queue depth above which smooth pacing gives up and drains the backlog.
const ENTER_QUEUE_DEPTH_LINES: usize = 8;
/// Age of the oldest queued line above which smooth pacing gives up.
const ENTER_OLDEST_AGE: Duration = Duration::from_millis(120);
/// Depth at or below which catch-up may start winding down.
const EXIT_QUEUE_DEPTH_LINES: usize = 2;
/// Oldest-line age at or below which catch-up may start winding down.
const EXIT_OLDEST_AGE: Duration = Duration::from_millis(40);
/// How long pressure must stay low before leaving catch-up.
const EXIT_HOLD: Duration = Duration::from_millis(250);
/// Cooldown after leaving catch-up, so the two modes do not flap at the
/// threshold. Severe backlog bypasses it.
const REENTER_CATCH_UP_HOLD: Duration = Duration::from_millis(250);
const SEVERE_QUEUE_DEPTH_LINES: usize = 64;
const SEVERE_OLDEST_AGE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChunkingMode {
    /// One queued line per commit tick.
    #[default]
    Smooth,
    /// Every queued line at once, until the backlog clears.
    CatchUp,
}

/// Pacing state machine shared by every stream of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkingPolicy {
    mode: ChunkingMode,
    /// When queue pressure first dropped below the exit thresholds.
    below_exit_since: Option<Instant>,
    /// When catch-up was last left, for the re-entry cooldown.
    left_catch_up_at: Option<Instant>,
}

impl ChunkingPolicy {
    pub fn mode(&self) -> ChunkingMode {
        self.mode
    }

    /// Returns how many queued lines to release now.
    pub fn decide(&mut self, queued: usize, oldest_age: Option<Duration>, now: Instant) -> usize {
        if queued == 0 {
            self.mode = ChunkingMode::Smooth;
            self.below_exit_since = None;
            return 0;
        }

        match self.mode {
            ChunkingMode::Smooth => {
                if self.should_enter_catch_up(queued, oldest_age, now) {
                    self.mode = ChunkingMode::CatchUp;
                    self.below_exit_since = None;
                    queued
                } else {
                    1
                }
            }
            ChunkingMode::CatchUp => {
                if self.should_exit_catch_up(queued, oldest_age, now) {
                    self.mode = ChunkingMode::Smooth;
                    self.below_exit_since = None;
                    self.left_catch_up_at = Some(now);
                    1
                } else {
                    queued
                }
            }
        }
    }

    fn should_enter_catch_up(
        &self,
        queued: usize,
        oldest_age: Option<Duration>,
        now: Instant,
    ) -> bool {
        let severe = queued >= SEVERE_QUEUE_DEPTH_LINES
            || oldest_age.is_some_and(|age| age >= SEVERE_OLDEST_AGE);
        if !severe
            && self
                .left_catch_up_at
                .is_some_and(|at| now.duration_since(at) < REENTER_CATCH_UP_HOLD)
        {
            return false;
        }
        queued >= ENTER_QUEUE_DEPTH_LINES
            || oldest_age.is_some_and(|age| age >= ENTER_OLDEST_AGE)
    }

    fn should_exit_catch_up(
        &mut self,
        queued: usize,
        oldest_age: Option<Duration>,
        now: Instant,
    ) -> bool {
        let calm = queued <= EXIT_QUEUE_DEPTH_LINES
            && oldest_age.is_none_or(|age| age <= EXIT_OLDEST_AGE);
        if !calm {
            self.below_exit_since = None;
            return false;
        }
        match self.below_exit_since {
            Some(since) => now.duration_since(since) >= EXIT_HOLD,
            None => {
                self.below_exit_since = Some(now);
                false
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedLine {
    line: Line<'static>,
    enqueued_at: Instant,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamController {
    revision: u64,
    raw_source: String,
    stable_prefix_len: usize,
    finalized: bool,
    truncated: bool,
    /// Lines rendered from the stable prefix and waiting for a commit tick.
    queue: VecDeque<QueuedLine>,
    /// Lines already released to the scrollback. The active cell skips them.
    released_lines: usize,
    /// Width the queued lines were rendered at. A different width invalidates
    /// them: wrapping decisions no longer hold.
    queue_width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamView<'a> {
    pub raw_source: &'a str,
    pub stable_prefix: &'a str,
    pub mutable_tail: &'a str,
    pub revision: u64,
    pub finalized: bool,
    pub truncated: bool,
}

pub const MAX_STREAM_SOURCE_CHARS: usize = 65_536;
const STREAM_TRUNCATED_MARKER: &str = "\n… truncated";

impl StreamController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_finalized(source: impl Into<String>) -> Self {
        let raw_source = source.into();
        Self {
            revision: 1,
            stable_prefix_len: raw_source.len(),
            raw_source,
            finalized: true,
            ..Self::default()
        }
    }

    pub fn push_delta(&mut self, delta: &str) {
        if delta.is_empty() || self.truncated {
            return;
        }
        let used = self.raw_source.chars().count();
        let remaining = MAX_STREAM_SOURCE_CHARS.saturating_sub(used);
        if remaining == 0 {
            self.raw_source.push_str(STREAM_TRUNCATED_MARKER);
            self.truncated = true;
        } else {
            self.raw_source.extend(delta.chars().take(remaining));
            if delta.chars().count() > remaining {
                self.raw_source.push_str(STREAM_TRUNCATED_MARKER);
                self.truncated = true;
            }
        }
        self.finalized = false;
        self.recompute_stable_prefix();
        self.revision = self.revision.saturating_add(1);
    }

    pub fn finalize(&mut self) {
        if self.finalized && self.stable_prefix_len == self.raw_source.len() {
            return;
        }
        self.finalized = true;
        self.stable_prefix_len = self.raw_source.len();
        self.revision = self.revision.saturating_add(1);
    }

    pub fn reset(&mut self) {
        self.raw_source.clear();
        self.stable_prefix_len = 0;
        self.finalized = false;
        self.truncated = false;
        self.queue.clear();
        self.released_lines = 0;
        self.revision = self.revision.saturating_add(1);
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn raw_source(&self) -> &str {
        &self.raw_source
    }

    pub fn stable_prefix(&self) -> &str {
        &self.raw_source[..self.stable_prefix_len]
    }

    pub fn mutable_tail(&self) -> &str {
        &self.raw_source[self.stable_prefix_len..]
    }

    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    pub fn view(&self) -> StreamView<'_> {
        StreamView {
            raw_source: self.raw_source(),
            stable_prefix: self.stable_prefix(),
            mutable_tail: self.mutable_tail(),
            revision: self.revision,
            finalized: self.finalized,
            truncated: self.truncated,
        }
    }

    /// Number of leading rendered lines already released to the scrollback.
    pub fn released_lines(&self) -> usize {
        self.released_lines
    }

    pub fn queued_lines(&self) -> usize {
        self.queue.len()
    }

    pub fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.queue
            .front()
            .map(|queued| now.saturating_duration_since(queued.enqueued_at))
    }

    /// Renders the stable prefix and queues whatever is not queued or released
    /// yet.
    ///
    /// `render` receives the whole stable prefix, not the new fragment: markdown
    /// is not decomposable line by line (a list, a fenced block or a quote only
    /// renders correctly with its opening in scope). Only the lines beyond what
    /// was already emitted are kept, which is what makes re-rendering the prefix
    /// idempotent.
    pub fn refill(
        &mut self,
        width: u16,
        now: Instant,
        render: impl FnOnce(&str) -> Vec<Line<'static>>,
    ) {
        if width != self.queue_width {
            // Requeue from scratch at the new width. Lines already released are
            // the terminal's now and are repaired by resize reflow, not here.
            self.queue.clear();
            self.queue_width = width;
        }
        if self.stable_prefix_len == 0 {
            return;
        }

        let emitted = self.released_lines + self.queue.len();
        let rendered = render(self.stable_prefix());
        if rendered.len() <= emitted {
            return;
        }
        for line in rendered.into_iter().skip(emitted) {
            self.queue.push_back(QueuedLine {
                line,
                enqueued_at: now,
            });
        }
    }

    /// Releases up to `count` queued lines, oldest first.
    pub fn release(&mut self, count: usize) -> Vec<Line<'static>> {
        let count = count.min(self.queue.len());
        let released: Vec<Line<'static>> = self
            .queue
            .drain(..count)
            .map(|queued| queued.line)
            .collect();
        self.released_lines += released.len();
        released
    }

    /// Releases everything still queued. Used when the stream ends: nothing is
    /// left to pace, and holding lines back would delay the final answer.
    pub fn release_all(&mut self) -> Vec<Line<'static>> {
        self.release(self.queue.len())
    }

    /// Forgets what was released, so the whole source is emitted again.
    ///
    /// Used by resize reflow: the rows in the scrollback were wrapped at the old
    /// width and are about to be discarded, so the cell owes them all over again.
    pub fn reset_release(&mut self) {
        self.queue.clear();
        self.released_lines = 0;
    }

    fn recompute_stable_prefix(&mut self) {
        if self.finalized {
            self.stable_prefix_len = self.raw_source.len();
            return;
        }

        let Some(last_newline) = self.raw_source.rfind('\n') else {
            self.stable_prefix_len = 0;
            return;
        };
        let candidate = last_newline + 1;
        self.stable_prefix_len = markdown_holdback_start(
            &self.raw_source,
            candidate,
            self.stable_prefix_len.min(candidate),
        )
        .unwrap_or(candidate);
    }
}

fn markdown_holdback_start(source: &str, candidate: usize, scan_start: usize) -> Option<usize> {
    if source.ends_with("\n\n") || source.ends_with("\r\n\r\n") {
        return None;
    }

    let table_start = scan_start + table_run_start(&source[scan_start..candidate])?;
    let tail_after_candidate = &source[candidate..];
    let prefix_last_nonempty = source[..candidate]
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty());

    let table_still_live = prefix_last_nonempty.is_some_and(is_table_like_line)
        || (!tail_after_candidate.trim().is_empty() && !source.ends_with('\n'));

    table_still_live.then_some(table_start)
}

fn table_run_start(prefix: &str) -> Option<usize> {
    let mut start = 0usize;
    let mut table_start = None;
    for part in prefix.split_inclusive('\n') {
        let line = part.trim_end_matches(['\r', '\n']).trim();
        if is_table_like_line(line) {
            table_start.get_or_insert(start);
        } else if line.is_empty() || table_start.is_some() {
            table_start = None;
        }
        start += part.len();
    }
    table_start
}

fn is_table_like_line(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let pipes = line.chars().filter(|ch| *ch == '|').count();
    pipes >= 2 || (line.contains('|') && line.chars().any(|ch| ch == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_delta_commits_to_stable_prefix() {
        let mut stream = StreamController::new();

        stream.push_delta("hello");
        assert_eq!(stream.stable_prefix(), "");
        assert_eq!(stream.mutable_tail(), "hello");

        stream.push_delta(" world\nnext");
        assert_eq!(stream.stable_prefix(), "hello world\n");
        assert_eq!(stream.mutable_tail(), "next");
    }

    #[test]
    fn incomplete_markdown_table_stays_in_tail_until_confirmed() {
        let mut stream = StreamController::new();

        stream.push_delta("| A | B |\n");
        stream.push_delta("|---|---|\n");
        stream.push_delta("| 1");

        assert_eq!(stream.stable_prefix(), "");
        assert!(stream.mutable_tail().contains("| A | B |"));

        stream.push_delta(" | 2 |\n\nconfirmed");
        assert_eq!(
            stream.stable_prefix(),
            "| A | B |\n|---|---|\n| 1 | 2 |\n\n"
        );
        assert_eq!(stream.mutable_tail(), "confirmed");
    }

    #[test]
    fn finalize_commits_incomplete_tail() {
        let mut stream = StreamController::new();
        stream.push_delta("| A | B |\n|---|---|\n| 1");

        stream.finalize();

        assert!(stream.is_finalized());
        assert_eq!(stream.stable_prefix(), stream.raw_source());
        assert_eq!(stream.mutable_tail(), "");
    }

    #[test]
    fn reset_drops_unfinalized_tail() {
        let mut stream = StreamController::new();
        stream.push_delta("draft");

        stream.reset();

        assert_eq!(stream.raw_source(), "");
        assert_eq!(stream.stable_prefix(), "");
        assert_eq!(stream.mutable_tail(), "");
        assert!(!stream.is_finalized());
    }

    #[test]
    fn source_is_bounded_after_large_delta() {
        let mut stream = StreamController::new();
        let marker_len = STREAM_TRUNCATED_MARKER.chars().count();

        stream.push_delta(&"x".repeat(MAX_STREAM_SOURCE_CHARS + 100));

        assert!(stream.view().truncated);
        assert_eq!(
            stream.raw_source().chars().count(),
            MAX_STREAM_SOURCE_CHARS + marker_len
        );
        assert!(stream.raw_source().contains("truncated"));

        let revision = stream.revision();
        stream.push_delta("ignored");
        assert_eq!(stream.revision(), revision);
        assert!(!stream.raw_source().contains("ignored"));
    }

    /// Renders one line per source line, so queue arithmetic is readable.
    fn render_lines(source: &str) -> Vec<Line<'static>> {
        source
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect()
    }

    #[test]
    fn refill_queues_only_lines_that_were_never_emitted() {
        let mut stream = StreamController::new();
        let now = Instant::now();

        stream.push_delta("un\ndeux\ntrois\n");
        stream.refill(80, now, render_lines);
        assert_eq!(stream.queued_lines(), 3);

        // Re-rendering the whole prefix must not duplicate what is already queued.
        stream.refill(80, now, render_lines);
        assert_eq!(stream.queued_lines(), 3);

        let released = stream.release(2);
        assert_eq!(released.len(), 2);
        assert_eq!(stream.released_lines(), 2);
        assert_eq!(stream.queued_lines(), 1);

        stream.push_delta("quatre\n");
        stream.refill(80, now, render_lines);
        assert_eq!(stream.queued_lines(), 2, "seule la nouvelle ligne s'ajoute");
    }

    #[test]
    fn a_width_change_requeues_the_unreleased_lines() {
        let mut stream = StreamController::new();
        let now = Instant::now();
        stream.push_delta("un\ndeux\n");
        stream.refill(80, now, render_lines);
        stream.release(1);

        stream.refill(40, now, render_lines);

        assert_eq!(
            stream.queued_lines(),
            1,
            "la file est reconstruite à la nouvelle largeur, sans rejouer le libéré"
        );
    }

    #[test]
    fn smooth_pacing_releases_one_line_per_tick() {
        let mut policy = ChunkingPolicy::default();
        let now = Instant::now();

        assert_eq!(policy.decide(3, Some(Duration::from_millis(10)), now), 1);
        assert_eq!(policy.mode(), ChunkingMode::Smooth);
    }

    #[test]
    fn a_deep_queue_switches_to_catch_up_and_drains_it() {
        let mut policy = ChunkingPolicy::default();
        let now = Instant::now();

        let released = policy.decide(ENTER_QUEUE_DEPTH_LINES, Some(Duration::ZERO), now);

        assert_eq!(released, ENTER_QUEUE_DEPTH_LINES);
        assert_eq!(policy.mode(), ChunkingMode::CatchUp);
    }

    #[test]
    fn an_old_queued_line_switches_to_catch_up() {
        let mut policy = ChunkingPolicy::default();
        let now = Instant::now();

        let released = policy.decide(2, Some(ENTER_OLDEST_AGE), now);

        assert_eq!(released, 2);
        assert_eq!(policy.mode(), ChunkingMode::CatchUp);
    }

    /// Leaving catch-up needs sustained calm: reacting to one quiet tick would
    /// make the two modes alternate every frame around the threshold.
    #[test]
    fn leaving_catch_up_requires_the_hold_window() {
        let mut policy = ChunkingPolicy::default();
        let start = Instant::now();
        policy.decide(ENTER_QUEUE_DEPTH_LINES, Some(Duration::ZERO), start);
        assert_eq!(policy.mode(), ChunkingMode::CatchUp);

        policy.decide(1, Some(Duration::ZERO), start);
        assert_eq!(policy.mode(), ChunkingMode::CatchUp, "trop tôt pour sortir");

        policy.decide(1, Some(Duration::ZERO), start + EXIT_HOLD);
        assert_eq!(policy.mode(), ChunkingMode::Smooth);
    }

    #[test]
    fn an_empty_queue_releases_nothing_and_resets_the_mode() {
        let mut policy = ChunkingPolicy::default();
        let now = Instant::now();
        policy.decide(ENTER_QUEUE_DEPTH_LINES, Some(Duration::ZERO), now);

        assert_eq!(policy.decide(0, None, now), 0);
        assert_eq!(policy.mode(), ChunkingMode::Smooth);
    }
}
