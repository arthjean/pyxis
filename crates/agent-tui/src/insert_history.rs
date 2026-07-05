//! Insertion of finalized history above the active terminal viewport.
//!
//! The legacy alt-screen renderer is still the default. The parity path uses
//! Ratatui inline viewports and falls back to legacy mode after any terminal
//! write error.

use std::io;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const MAX_PENDING_HISTORY_LINES: usize = 4096;
const MAX_PENDING_HISTORY_LINE_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertHistoryMode {
    Legacy,
    InlineScrollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedHistoryLine(String);

impl SanitizedHistoryLine {
    pub fn new(line: impl AsRef<str>) -> Self {
        Self(strip_terminal_controls(line.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHistoryInsert {
    pub lines: Vec<SanitizedHistoryLine>,
    render_lines: Vec<Line<'static>>,
    pub mode: InsertHistoryMode,
}

impl PendingHistoryInsert {
    pub fn new<I, S>(lines: I, mode: InsertHistoryMode) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let raw_lines = sanitize_lines(lines);
        let render_lines = raw_lines
            .iter()
            .map(|line| Line::raw(line.as_str().to_string()))
            .collect();
        Self {
            lines: raw_lines,
            render_lines,
            mode,
        }
    }

    pub fn legacy<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(lines, InsertHistoryMode::Legacy)
    }

    pub fn inline_scrollback<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(lines, InsertHistoryMode::InlineScrollback)
    }

    pub fn from_render_lines<I>(lines: I, mode: InsertHistoryMode) -> Self
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        let mut raw_lines = Vec::new();
        let mut render_lines = Vec::new();
        for line in lines.into_iter().take(MAX_PENDING_HISTORY_LINES) {
            let (raw, render) = sanitize_render_line(line);
            raw_lines.push(raw);
            render_lines.push(render);
        }
        Self {
            lines: raw_lines,
            render_lines,
            mode,
        }
    }

    pub fn legacy_lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        Self::from_render_lines(lines, InsertHistoryMode::Legacy)
    }

    pub fn inline_scrollback_lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        Self::from_render_lines(lines, InsertHistoryMode::InlineScrollback)
    }

    pub fn height(&self) -> u16 {
        self.lines.len().min(u16::MAX as usize) as u16
    }

    fn ratatui_lines(&self) -> Vec<Line<'static>> {
        self.render_lines.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryInsertError {
    message: String,
}

impl HistoryInsertError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for HistoryInsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HistoryInsertError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryInserter {
    mode: InsertHistoryMode,
    fallback_notice: Option<String>,
}

impl HistoryInserter {
    pub fn new(mode: InsertHistoryMode) -> Self {
        Self {
            mode,
            fallback_notice: None,
        }
    }

    pub fn mode(&self) -> InsertHistoryMode {
        self.mode
    }

    pub fn fallback_notice(&self) -> Option<&str> {
        self.fallback_notice.as_deref()
    }

    pub fn insert<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        insert: &PendingHistoryInsert,
    ) -> Result<(), HistoryInsertError> {
        if self.mode == InsertHistoryMode::Legacy
            || insert.mode == InsertHistoryMode::Legacy
            || insert.lines.is_empty()
        {
            return Ok(());
        }

        let lines = insert.ratatui_lines();
        terminal
            .insert_before(insert.height(), |buf| {
                Paragraph::new(lines).render(buf.area, buf);
            })
            .map_err(|error| self.record_write_error(error))
    }

    pub fn record_write_error(&mut self, error: impl Into<io::Error>) -> HistoryInsertError {
        let error = error.into();
        self.mode = InsertHistoryMode::Legacy;
        let message = format!("Terminal scrollback fallback active: {error}");
        self.fallback_notice = Some(message.clone());
        HistoryInsertError { message }
    }
}

fn sanitize_lines<I, S>(lines: I) -> Vec<SanitizedHistoryLine>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    lines
        .into_iter()
        .take(MAX_PENDING_HISTORY_LINES)
        .map(|line| {
            SanitizedHistoryLine::new(truncate_chars(
                line.as_ref(),
                MAX_PENDING_HISTORY_LINE_CHARS,
            ))
        })
        .collect()
}

fn truncate_chars(line: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in line.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn strip_terminal_controls(line: &str) -> String {
    line.chars()
        .filter(|ch| !is_terminal_control(*ch))
        .collect()
}

fn sanitize_render_line(line: Line<'static>) -> (SanitizedHistoryLine, Line<'static>) {
    let mut plain = String::new();
    let mut spans = Vec::new();
    let mut used = 0usize;
    let style = line.style;
    for span in line.spans {
        if used >= MAX_PENDING_HISTORY_LINE_CHARS {
            break;
        }
        let clean = strip_terminal_controls(span.content.as_ref());
        let remaining = MAX_PENDING_HISTORY_LINE_CHARS.saturating_sub(used);
        let content = truncate_chars(&clean, remaining);
        used += content.chars().count();
        plain.push_str(&content);
        spans.push(Span::styled(content, span.style));
    }
    let render = Line::from(spans).style(style);
    (SanitizedHistoryLine::new(plain), render)
}

fn is_terminal_control(ch: char) -> bool {
    matches!(ch, '\u{1b}' | '\u{9b}' | '\u{7f}') || (ch.is_control() && ch != '\t')
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    #[test]
    fn inline_scrollback_sanitizes_terminal_controls() {
        let insert = PendingHistoryInsert::inline_scrollback(["ok\u{1b}[31mred\u{7}"]);

        assert_eq!(insert.lines[0].as_str(), "ok[31mred");
        assert_eq!(insert.mode, InsertHistoryMode::InlineScrollback);
    }

    #[test]
    fn inserter_writes_above_inline_viewport() {
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(1),
            },
        )
        .expect("inline test terminal");
        let insert = PendingHistoryInsert::inline_scrollback(["line 1", "line 2"]);
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);

        inserter
            .insert(&mut terminal, &insert)
            .expect("inline insert succeeds");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("prompt"), frame.area()))
            .expect("draw prompt");

        terminal.backend().assert_buffer_lines([
            "line 1              ",
            "line 2              ",
            "prompt              ",
            "                    ",
            "                    ",
        ]);
        assert_eq!(inserter.mode(), InsertHistoryMode::InlineScrollback);
    }

    #[test]
    fn write_error_switches_to_legacy_and_records_notice() {
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);
        let err = inserter.record_write_error(io::Error::other("boom"));

        assert_eq!(inserter.mode(), InsertHistoryMode::Legacy);
        assert!(err.message().contains("fallback active"));
        assert_eq!(inserter.fallback_notice(), Some(err.message()));
    }

    #[test]
    fn pending_insert_bounds_line_count_and_width() {
        let long = "x".repeat(MAX_PENDING_HISTORY_LINE_CHARS + 10);
        let insert = PendingHistoryInsert::inline_scrollback(
            (0..MAX_PENDING_HISTORY_LINES + 10).map(|_| long.as_str()),
        );

        assert_eq!(insert.lines.len(), MAX_PENDING_HISTORY_LINES);
        assert!(insert.lines[0].as_str().ends_with('…'));
    }

    #[test]
    fn styled_lines_survive_pending_insert_sanitization() {
        use ratatui::style::{Color, Style};

        let insert = PendingHistoryInsert::inline_scrollback_lines([Line::from(Span::styled(
            "accent",
            Style::default().fg(Color::Rgb(0x6c, 0xcb, 0xff)),
        ))]);
        let lines = insert.ratatui_lines();

        assert_eq!(insert.lines[0].as_str(), "accent");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(Color::Rgb(0x6c, 0xcb, 0xff))
        );
    }
}
