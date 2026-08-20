//! `read` tool: reads a workspace file with line numbers. Read-only,
//! concurrency-safe, untrusted output (the content read can carry an injection,
//! OWASP LLM01). US-011 AC1/AC3.
//!
//! **The cap bounds the WINDOW, not the file (US-077).** The file is streamed:
//! lines before `offset` are counted and dropped without ever being kept, so
//! any line of a 10 MiB spilled artifact is reachable by paging with `offset`
//! and `limit`. Reading a fixed prefix from byte 0, as this tool used to, made
//! the pagination hint a lie past [`MAX_BYTES`] and left 80% of a spilled
//! output unreadable by the very tool the spill notice points at.
//!
//! No parameter is added: `offset` in lines is enough to reach any byte by
//! successive steps, and a byte offset would spend a permanent slot in the
//! model-visible contract for nothing. The one case lines cannot address is a
//! single line longer than the window, which is named by its own hint instead
//! of being hidden.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ToolError;
use crate::path::{confine, ensure_existing_path_no_links};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Bytes the RENDERED WINDOW may occupy. The file itself has no size limit any
/// more; this only bounds what one call puts into the context.
const MAX_BYTES: usize = 2_000_000;

/// Bytes read from disk per iteration. Only this much, plus the window, is ever
/// resident: the memory cost of reading a 10 MiB file is the window, not the
/// file.
const CHUNK_BYTES: usize = 64 * 1024;

/// Bytes a rendered line costs beyond its own content: the number field, the
/// tab and the newline. Reserved per line so the budget check stays ahead of
/// the formatting instead of discovering the overflow after it.
const LINE_OVERHEAD: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
    /// Start line (1-indexed). Default: 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Max number of lines. Default: everything.
    #[serde(default)]
    pub limit: Option<usize>,
}

pub struct Read;

#[async_trait]
impl Tool for Read {
    type Input = ReadInput;

    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> String {
        "Read a workspace text file and return its contents prefixed with line \
         numbers. Parameters: path (relative to the workspace), offset \
         (1-indexed start line, optional), limit (line count, optional)."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the workspace." },
                "offset": { "type": ["integer", "null"], "minimum": 1, "description": "Start line (1-indexed), or null." },
                "limit": { "type": ["integer", "null"], "minimum": 1, "description": "Maximum number of lines, or null." }
            },
            "required": ["path", "offset", "limit"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        ensure_existing_path_no_links(&ctx.workspace, &path, &input.path)?;
        let display = input.path.clone();
        let start = input.offset.unwrap_or(1).max(1);
        let limit = input.limit;

        // Blocking FS moved off the async runtime, like `grep` and `glob`: the
        // scan walks the whole file to count its lines, which is what makes the
        // continuation hint and the out-of-bounds message exact.
        let rendered = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let file =
                std::fs::File::open(&path).map_err(|e| ToolError::Io(format!("{display}: {e}")))?;
            let meta = file
                .metadata()
                .map_err(|e| ToolError::Io(format!("{display}: {e}")))?;
            if meta.is_dir() {
                return Err(ToolError::Rejected(format!(
                    "{display} is a directory, not a file"
                )));
            }
            match scan(std::io::BufReader::new(file), start, limit)
                .map_err(|e| ToolError::Io(format!("{display}: {e}")))?
            {
                Scan::Binary => Err(ToolError::Rejected(format!(
                    "{display} appears to be a binary file (NUL bytes)"
                ))),
                Scan::Window(w) => Ok(w.render()),
            }
        })
        .await
        .map_err(|e| ToolError::Io(format!("{}: read task: {e}", input.path)))??;

        Ok(ToolOutput::text(rendered))
    }
}

/// Outcome of one streaming pass over a file.
enum Scan {
    /// A NUL byte was met anywhere in the file: not text, refused as before.
    Binary,
    Window(Window),
}

/// The emitted window and everything the hints need to be exact.
///
/// Only the current line and the rendered body are held, both bounded by
/// [`MAX_BYTES`]; every other line costs a counter.
#[derive(Default)]
struct Window {
    start: usize,
    limit: Option<usize>,
    /// Numbered lines, ready to be returned.
    body: String,
    /// Kept bytes of the line being read, empty while lines are skipped.
    line: Vec<u8>,
    /// The current line did not fit in the remaining budget.
    line_cut: bool,
    emitted: usize,
    first_line: usize,
    last_line: usize,
    /// Lines in the whole file, counted even after the window closed.
    total: usize,
    /// The window closed because the budget ran out, not because of `limit`.
    window_full: bool,
    /// One line alone exceeds the budget: its head is shown and `offset` cannot
    /// address the rest of it. Named rather than silently dropped.
    split_line: Option<usize>,
    limit_reached: bool,
}

impl Window {
    fn new(start: usize, limit: Option<usize>) -> Self {
        Self {
            start,
            limit,
            ..Default::default()
        }
    }

    /// Whether the line being read (number `total + 1`) still belongs to the
    /// window.
    fn keeping(&self) -> bool {
        !self.window_full && !self.limit_reached && self.total + 1 >= self.start
    }

    fn keep(&mut self, bytes: &[u8]) {
        if !self.keeping() {
            return;
        }
        let used = self.body.len() + self.line.len() + LINE_OVERHEAD;
        let room = MAX_BYTES.saturating_sub(used);
        // `room == 0` closes the window even for an EMPTY line: its frame still
        // costs bytes, and a run of blank lines arriving once the budget is
        // spent would otherwise grow the body without ever tripping the cut.
        if bytes.len() > room || room == 0 {
            self.line.extend_from_slice(&bytes[..room]);
            self.line_cut = true;
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn finish_line(&mut self) {
        let lineno = self.total + 1;
        let kept = self.keeping();
        self.total = lineno;
        if !kept {
            self.line.clear();
            self.line_cut = false;
            return;
        }
        if self.line_cut && self.emitted > 0 {
            // A line that does not fit is left WHOLE for the next call: cutting
            // it here would make the next `offset` skip its tail, and the hint
            // promises that following it step by step walks the whole file.
            self.line.clear();
            self.line_cut = false;
            self.window_full = true;
            return;
        }
        let mut bytes = std::mem::take(&mut self.line);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let text = String::from_utf8_lossy(&bytes);
        self.body.push_str(&format!("{lineno:>6}\t{text}\n"));
        self.emitted += 1;
        if self.first_line == 0 {
            self.first_line = lineno;
        }
        self.last_line = lineno;
        if self.line_cut {
            self.line_cut = false;
            self.window_full = true;
            self.split_line = Some(lineno);
        }
        if self.limit.is_some_and(|l| self.emitted >= l) {
            self.limit_reached = true;
        }
    }

    /// The window plus the hint that describes it. Every hint states the range
    /// rendered and the next `offset`, so a model following them reaches the
    /// last line of a file of any size.
    fn render(&self) -> String {
        if self.body.is_empty() {
            return if self.total == 0 {
                "(empty file)".to_string()
            } else {
                format!(
                    "[range out of bounds: offset={} > {} lines]",
                    self.start, self.total
                )
            };
        }
        let mut out = self.body.clone();
        if let Some(lineno) = self.split_line {
            out.push_str(&format!(
                "[line {lineno} of {} exceeds the {MAX_BYTES}-byte window; only its head \
                 is shown, and offset addresses lines, not its remainder",
                self.total
            ));
            // The remainder of THAT line is unreachable, the rest of the file
            // is not: without this the walk would stop here, and the hint
            // promises the opposite.
            if lineno < self.total {
                out.push_str(&format!("; offset={} to continue past it", lineno + 1));
            }
            out.push(']');
            return out;
        }
        if self.window_full {
            out.push_str(&format!(
                "[lines {}-{} of {}; window capped at {MAX_BYTES} bytes; offset={} to \
                 continue]",
                self.first_line,
                self.last_line,
                self.total,
                self.last_line + 1
            ));
        } else if self.limit_reached && self.last_line < self.total {
            out.push_str(&format!(
                "[lines {}-{} of {}; offset={} to continue]",
                self.first_line,
                self.last_line,
                self.total,
                self.last_line + 1
            ));
        }
        out
    }
}

/// Streams `reader`, emitting at most `limit` lines from `start` and counting
/// every line. Pure with respect to the filesystem: any reader works, which is
/// what makes the window testable without a 10 MiB file on disk.
fn scan<R: std::io::Read>(
    mut reader: R,
    start: usize,
    limit: Option<usize>,
) -> std::io::Result<Scan> {
    let mut w = Window::new(start, limit);
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut pending = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if chunk.contains(&0) {
            return Ok(Scan::Binary);
        }
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            w.keep(&rest[..pos]);
            w.finish_line();
            pending = false;
            rest = &rest[pos + 1..];
        }
        if !rest.is_empty() {
            w.keep(rest);
            pending = true;
        }
    }
    // A file not ending with a newline still has a last line, exactly as
    // `str::lines` sees it.
    if pending {
        w.finish_line();
    }
    Ok(Scan::Window(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole pipeline minus the filesystem: what the tool returns for this
    /// text, this offset and this limit.
    fn read_text(text: &str, start: usize, limit: Option<usize>) -> String {
        match scan(std::io::Cursor::new(text.as_bytes()), start, limit).expect("in-memory scan") {
            // Never reached by the texts below; a marker keeps the helper
            // total instead of pulling a denied `panic!` into the crate.
            Scan::Binary => "(binary)".to_string(),
            Scan::Window(w) => w.render(),
        }
    }

    fn text5() -> &'static str {
        "l1\nl2\nl3\nl4\nl5\n"
    }

    #[test]
    fn a_full_read_has_no_hint() {
        let out = read_text(text5(), 1, None);
        assert!(out.contains("     1\tl1"));
        assert!(out.contains("     5\tl5"));
        assert!(
            !out.contains("offset="),
            "full read should have no hint: {out}"
        );
    }

    #[test]
    fn a_limit_truncation_emits_a_continuation_hint() {
        let out = read_text(text5(), 1, Some(2));
        assert!(out.contains("     1\tl1"));
        assert!(out.contains("     2\tl2"));
        assert!(!out.contains("\tl3"));
        assert!(
            out.contains("[lines 1-2 of 5; offset=3 to continue]"),
            "pagination hint expected: {out}"
        );
    }

    #[test]
    fn a_limit_that_reaches_the_last_line_emits_no_hint() {
        let out = read_text(text5(), 4, Some(2));
        assert!(out.contains("     5\tl5"));
        assert!(
            !out.contains("to continue"),
            "nothing left to continue with: {out}"
        );
    }

    #[test]
    fn an_out_of_range_offset_reports_the_real_line_count() {
        let out = read_text(text5(), 99, None);
        assert!(
            out.contains("[range out of bounds: offset=99 > 5 lines]"),
            "out-of-range hint expected: {out}"
        );
    }

    #[test]
    fn an_offset_past_the_window_cap_still_reaches_the_end_of_the_file() {
        // The old prefix read stopped at MAX_BYTES from byte 0, so these lines
        // did not exist for any offset. Streaming makes the last one reachable.
        let line = format!("{}\n", "x".repeat(1024));
        let lines = MAX_BYTES / 1024 + 200;
        let mut text = line.repeat(lines);
        text.push_str("LAST\n");
        let out = read_text(&text, lines + 1, None);
        assert!(
            out.contains(&format!("{}\tLAST", lines + 1)),
            "the last line must be reachable: {out}"
        );
        assert!(!out.contains("xxxx"), "no earlier line is kept: {out}");
    }

    #[test]
    fn a_window_capped_by_bytes_names_the_next_offset() {
        let line = format!("{}\n", "y".repeat(1000));
        let text = line.repeat(MAX_BYTES / 1000 + 10);
        let out = read_text(&text, 1, None);
        assert!(
            out.len() <= MAX_BYTES + 256,
            "window bounded: {}",
            out.len()
        );
        let next = out
            .rsplit("offset=")
            .next()
            .and_then(|s| s.split(' ').next())
            .map(str::to_string)
            .expect("a continuation offset");
        assert!(
            out.contains("window capped at"),
            "the cap must be named: {out}"
        );
        // Following the hint reaches the rest: nothing is lost between windows.
        let rest = read_text(&text, next.parse().expect("a line number"), None);
        assert!(rest.contains("\tyyy"), "the tail is reachable: {rest}");
    }

    #[test]
    fn a_single_line_longer_than_the_window_says_so() {
        let text = format!("{}\n", "z".repeat(MAX_BYTES + 4096));
        let out = read_text(&text, 1, None);
        assert!(
            out.contains("exceeds the") && out.contains("only its head is"),
            "an unreachable remainder must be named: {out}"
        );
    }

    #[test]
    fn the_file_stays_walkable_past_a_line_longer_than_the_window() {
        let mut text = format!("{}\n", "z".repeat(MAX_BYTES + 4096));
        text.push_str("AFTER\n");
        let out = read_text(&text, 1, None);
        assert!(
            out.contains("offset=2 to continue past it"),
            "the next line must stay addressable: {}",
            &out[out.len() - 200..]
        );
        assert!(read_text(&text, 2, None).contains("     2\tAFTER"));
    }

    #[test]
    fn a_run_of_blank_lines_cannot_grow_the_window_past_the_cap() {
        // The frame of a blank line still costs bytes: once the budget is
        // spent, an empty line must close the window like any other.
        let text = "\n".repeat(1_000_000);
        let out = read_text(&text, 1, None);
        assert!(
            out.len() <= MAX_BYTES + 256,
            "the window must stay capped: {} bytes",
            out.len()
        );
        assert!(out.contains("window capped at"), "the cap must be named");
    }

    #[test]
    fn an_empty_file_reports_empty() {
        assert_eq!(read_text("", 1, None), "(empty file)");
    }

    #[test]
    fn a_last_line_without_a_newline_is_still_a_line() {
        let out = read_text("a\nb", 1, None);
        assert!(out.contains("     2\tb"), "trailing line expected: {out}");
    }

    #[test]
    fn a_nul_byte_anywhere_marks_the_file_binary() {
        let mut text = "a\n".repeat(4096).into_bytes();
        text.push(0);
        assert!(matches!(
            scan(std::io::Cursor::new(text), 1, None).expect("scan"),
            Scan::Binary
        ));
    }

    #[test]
    fn a_crlf_line_loses_its_carriage_return() {
        let out = read_text("a\r\nb\r\n", 1, None);
        assert!(out.contains("     1\ta\n"), "CR stripped: {out:?}");
    }
}
