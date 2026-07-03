//! Stable-prefix plus mutable-tail streaming for the parity transcript path.
//!
//! The controller owns the raw assistant markdown source. It exposes a stable
//! prefix that can be treated as committed for rendering purposes, and a mutable
//! tail that remains live until a newline-safe boundary or finalization.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamController {
    revision: u64,
    raw_source: String,
    stable_prefix_len: usize,
    finalized: bool,
    truncated: bool,
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
            truncated: false,
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
}
