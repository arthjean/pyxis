//! Writes finalized history rows into the terminal scrollback.
//!
//! Finalized chat history is not a retained widget: it is written once, above
//! the viewport, and from then on the terminal owns it. That is what makes the
//! native scrollback, the terminal's own search and its text selection work on
//! the transcript.
//!
//! The insertion is an escape-sequence operation rather than a ratatui render:
//! the viewport gives rows back from its top, or a scrolling region pushes the
//! rows above it up into the scrollback, then the freed rows are painted.
//!
//! The row-splitting strategy is derived from `ratatui::Terminal::insert_before`
//! (MIT); see the licence header in [`crate::custom_terminal`].

use std::io;

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::custom_terminal::Terminal;
use crate::terminal_hyperlinks::{
    HyperlinkLine, TerminalHyperlink, annotate_web_urls_in_line, plain_hyperlink_lines,
    visible_lines,
};

const MAX_PENDING_HISTORY_LINES: usize = 4096;
const MAX_PENDING_HISTORY_LINE_CHARS: usize = 4096;
/// Bound on a link destination, which reaches the terminal as an escape
/// sequence payload rather than as displayed text.
const MAX_HYPERLINK_CHARS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertHistoryMode {
    /// The scrollback path is unavailable: history stays in the viewport.
    Legacy,
    InlineScrollback,
}

/// A history line stripped of terminal control sequences.
///
/// History rows come from model output, tool output and file contents. Writing
/// them to the terminal is the one place where an escape sequence in that
/// content would be interpreted rather than displayed.
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
    /// Rendered rows plus the destinations to mark them with. History is the one
    /// place a URL must survive as a link: once written, the terminal owns the
    /// row and nothing can annotate it after the fact.
    render_lines: Vec<HyperlinkLine>,
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
            .map(|line| annotate_web_urls_in_line(Line::raw(line.as_str().to_string())))
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

    pub fn legacy_hyperlink_lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = HyperlinkLine>,
    {
        Self::from_hyperlink_lines(lines, InsertHistoryMode::Legacy)
    }

    pub fn inline_scrollback_hyperlink_lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = HyperlinkLine>,
    {
        Self::from_hyperlink_lines(lines, InsertHistoryMode::InlineScrollback)
    }

    pub fn is_empty(&self) -> bool {
        self.render_lines.is_empty()
    }

    pub fn hyperlink_lines(&self) -> Vec<HyperlinkLine> {
        self.render_lines.clone()
    }

    pub fn ratatui_lines(&self) -> Vec<Line<'static>> {
        visible_lines(self.render_lines.clone())
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
            || insert.is_empty()
        {
            return Ok(());
        }

        insert_history_lines(terminal, insert.hyperlink_lines())
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

/// Writes `lines` immediately above the viewport, scrolling whatever is needed.
///
/// Three regimes, in order:
///
/// 1. The viewport is taller than its content needs: it gives rows back from the
///    top and history is painted into them. Nothing leaves the screen, and the
///    viewport keeps touching the bottom, which is where the composer lives.
/// 2. The viewport cannot shrink further: scroll the rows above it up. What
///    leaves the top of the screen enters the scrollback, which is the point.
/// 3. Nothing above and nothing to give: borrow the first viewport row, paint
///    one line into it, scroll that single row away, repeat.
pub fn insert_history_lines<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<HyperlinkLine>,
) -> io::Result<()> {
    let screen = terminal.size()?;
    terminal.record_screen_size(screen);
    if screen.width == 0 || screen.height == 0 || lines.is_empty() {
        return Ok(());
    }

    let Some(buffer) = render_history_buffer(&lines, screen.width) else {
        return Ok(());
    };
    let rows = buffer.area.height;

    let mut painted = 0u16;
    let mut remaining = rows;

    // The viewport is anchored to the bottom of the screen, so it never has free
    // rows below it: the room for history comes from its own top. What it gives
    // away here is taken back by `anchor_viewport` on the next draw if the
    // content still needs it.
    let viewport = terminal.viewport_area;
    if viewport.height > 1 {
        let to_draw = remaining.min(viewport.height - 1);
        draw_rows(terminal, &buffer, painted, to_draw, viewport.top())?;
        terminal.set_viewport_area(Rect {
            y: viewport.top() + to_draw,
            height: viewport.height - to_draw,
            ..viewport
        });
        painted += to_draw;
        remaining -= to_draw;
    }

    let viewport_top = terminal.viewport_area.top();
    while remaining > 0 && viewport_top > 0 {
        let to_draw = remaining.min(viewport_top);
        terminal
            .backend_mut()
            .scroll_region_up(0..viewport_top, to_draw)?;
        draw_rows(terminal, &buffer, painted, to_draw, viewport_top - to_draw)?;
        painted += to_draw;
        remaining -= to_draw;
    }

    while remaining > 0 {
        draw_rows(terminal, &buffer, painted, 1, 0)?;
        terminal.backend_mut().scroll_region_up(0..1, 1)?;
        painted += 1;
        remaining -= 1;
    }

    // Everything painted sits above the viewport, and `note_history_rows` clamps
    // to what is still on screen: a resize reflow may only rewrite those.
    terminal.note_history_rows(painted);

    // Rows 1 and 3 wrote outside the double-buffer's knowledge, and row 3 wrote
    // over the viewport itself. Drop the previous frame so the next draw repaints
    // every cell instead of diffing against a screen that moved underneath it.
    terminal.invalidate_viewport();
    terminal.backend_mut().flush()
}

/// Renders the history rows into an off-screen buffer, one source line at a
/// time, and marks the link destinations while their columns are still known.
///
/// Rendering line by line rather than as one paragraph is what keeps the link
/// columns meaningful: a source line that wraps occupies several rows, and its
/// columns no longer describe any single one of them. Such a line keeps its text
/// and loses only its links.
fn render_history_buffer(lines: &[HyperlinkLine], width: u16) -> Option<Buffer> {
    let mut heights = Vec::with_capacity(lines.len());
    let mut total = 0usize;
    for line in lines {
        let height = Paragraph::new(line.line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1);
        heights.push(height);
        total = total.saturating_add(height);
        if total >= MAX_PENDING_HISTORY_LINES {
            break;
        }
    }
    let rows = u16::try_from(total.min(MAX_PENDING_HISTORY_LINES)).unwrap_or(u16::MAX);
    if rows == 0 {
        return None;
    }

    let mut buffer = Buffer::empty(Rect::new(0, 0, width, rows));
    let mut y = 0u16;
    for (line, height) in lines.iter().zip(heights) {
        let height = u16::try_from(height).unwrap_or(u16::MAX).min(rows - y);
        if height == 0 {
            break;
        }
        Paragraph::new(line.line.clone())
            .wrap(Wrap { trim: false })
            .render(Rect::new(0, y, width, height), &mut buffer);
        if height == 1 {
            mark_hyperlinks(&mut buffer, y, &line.hyperlinks);
        }
        y += height;
        if y >= rows {
            break;
        }
    }
    Some(buffer)
}

/// Wraps the linked columns of `row` in OSC 8, by folding the escape sequence
/// into the symbol of the first and last cell of the run.
///
/// The sequence has to live in the cell text because the write path is a cell
/// diff, with no room for out-of-band output. It carries no width, so layout is
/// unaffected.
fn mark_hyperlinks(buffer: &mut Buffer, row: u16, hyperlinks: &[TerminalHyperlink]) {
    let width = usize::from(buffer.area.width);
    for hyperlink in hyperlinks {
        let start = hyperlink.columns.start;
        let end = hyperlink.columns.end;
        if start >= end || end > width {
            continue;
        }
        // The destination comes from model output: an escape inside it would
        // end the sequence early and let the rest run as terminal commands.
        let destination = strip_terminal_controls(&hyperlink.destination);
        if destination.is_empty() || destination.chars().count() > MAX_HYPERLINK_CHARS {
            continue;
        }
        let (Ok(start_x), Ok(end_x)) = (u16::try_from(start), u16::try_from(end - 1)) else {
            continue;
        };
        let opening = format!(
            "\x1b]8;;{destination}\x07{}",
            buffer[(start_x, row)].symbol()
        );
        buffer[(start_x, row)].set_symbol(&opening);
        let closing = format!("{}\x1b]8;;\x07", buffer[(end_x, row)].symbol());
        buffer[(end_x, row)].set_symbol(&closing);
    }
}

/// Paints `count` rows of `buffer`, starting at buffer row `from`, at screen row
/// `at`. Trailing blanks are dropped so terminal text selection does not pick up
/// a run of spaces at the end of every history line.
fn draw_rows<B: Backend>(
    terminal: &mut Terminal<B>,
    buffer: &Buffer,
    from: u16,
    count: u16,
    at: u16,
) -> io::Result<()> {
    let width = buffer.area.width;
    if width == 0 || count == 0 {
        return Ok(());
    }
    let mut updates = Vec::new();
    for row in 0..count {
        let source_y = from + row;
        if source_y >= buffer.area.height {
            break;
        }
        let last_used = (0..width)
            .rev()
            .find(|x| {
                let cell = &buffer[(*x, source_y)];
                cell.symbol() != " " || cell.style() != ratatui::style::Style::default()
            })
            .map(|x| x + 1)
            .unwrap_or(0);
        for x in 0..last_used {
            updates.push((x, at + row, &buffer[(x, source_y)]));
        }
    }
    terminal.backend_mut().draw(updates.into_iter())?;
    terminal.backend_mut().flush()
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
    use ratatui::layout::{Position, Size};

    /// Buffer -> text, so an assertion can name what the screen actually shows.
    fn dump(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn terminal(width: u16, height: u16, viewport: Rect) -> Terminal<TestBackend> {
        let mut terminal = Terminal::with_geometry(
            TestBackend::new(width, height),
            Size::new(width, height),
            Position { x: 0, y: 0 },
        );
        terminal.set_viewport_area(viewport);
        terminal
    }

    #[test]
    fn inline_scrollback_sanitizes_terminal_controls() {
        let insert = PendingHistoryInsert::inline_scrollback(["ok\u{1b}[31mred\u{7}"]);

        assert_eq!(insert.lines[0].as_str(), "ok[31mred");
        assert_eq!(insert.mode, InsertHistoryMode::InlineScrollback);
    }

    /// History takes rows from the top of the viewport, never from below it: the
    /// composer must not move up the screen as the transcript grows.
    #[test]
    fn history_takes_rows_from_the_top_of_the_viewport() {
        let mut terminal = terminal(20, 5, Rect::new(0, 0, 20, 5));
        let insert = PendingHistoryInsert::inline_scrollback(["line 1", "line 2"]);
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);

        inserter
            .insert(&mut terminal, &insert)
            .expect("insertion inline");

        assert_eq!(terminal.viewport_area.y, 2, "le viewport cède par le haut");
        assert_eq!(terminal.viewport_area.bottom(), 5, "le bord bas ne bouge pas");
        let screen = dump(terminal.backend().buffer());
        assert!(screen.contains("line 1"), "{screen}");
        assert!(screen.contains("line 2"), "{screen}");
    }

    /// Once the viewport is anchored at the bottom, history scrolls the rows
    /// above it up rather than moving the viewport.
    #[test]
    fn history_scrolls_rows_above_a_bottom_anchored_viewport() {
        let mut terminal = terminal(20, 4, Rect::new(0, 3, 20, 1));
        let insert = PendingHistoryInsert::inline_scrollback(["a", "b"]);
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);

        inserter
            .insert(&mut terminal, &insert)
            .expect("insertion inline");

        assert_eq!(terminal.viewport_area.y, 3, "le viewport reste ancré en bas");
        let screen = dump(terminal.backend().buffer());
        assert!(screen.contains('a'), "{screen}");
        assert!(screen.contains('b'), "{screen}");
    }

    /// Resize reflow may only rewrite what this session put on screen. The rows
    /// the user already had in their terminal are not ours to clear.
    #[test]
    fn only_the_rows_this_session_wrote_are_reclaimed() {
        let mut terminal = terminal(20, 6, Rect::new(0, 0, 20, 6));
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);
        inserter
            .insert(
                &mut terminal,
                &PendingHistoryInsert::inline_scrollback(["une", "deux"]),
            )
            .expect("insertion inline");

        assert_eq!(terminal.visible_history_rows(), 2);
        let freed = terminal.clear_owned_history().expect("libération");

        assert_eq!(freed, 2);
        assert_eq!(terminal.viewport_area.y, 0, "le viewport revient à leur place");
        assert_eq!(terminal.visible_history_rows(), 0);
    }

    /// Rows pushed past the top of the screen belong to the terminal's
    /// scrollback: counting them would let a reflow clear rows it cannot rewrite.
    #[test]
    fn rows_scrolled_off_screen_stop_being_ours() {
        let mut terminal = terminal(20, 3, Rect::new(0, 2, 20, 1));
        let mut inserter = HistoryInserter::new(InsertHistoryMode::InlineScrollback);

        inserter
            .insert(
                &mut terminal,
                &PendingHistoryInsert::inline_scrollback(["une", "deux", "trois", "quatre"]),
            )
            .expect("insertion inline");

        assert_eq!(
            terminal.visible_history_rows(),
            terminal.viewport_area.top(),
            "au plus les rangées encore au-dessus du viewport"
        );
    }

    /// A URL written to the scrollback must stay clickable: nothing can annotate
    /// the row once the terminal owns it.
    #[test]
    fn a_url_reaches_the_scrollback_wrapped_in_osc_8() {
        let mut terminal = terminal(40, 6, Rect::new(0, 0, 40, 6));
        let insert = PendingHistoryInsert::inline_scrollback(["voir https://example.com ici"]);

        HistoryInserter::new(InsertHistoryMode::InlineScrollback)
            .insert(&mut terminal, &insert)
            .expect("insertion inline");

        let screen = dump(terminal.backend().buffer());
        assert!(
            screen.contains("\u{1b}]8;;https://example.com\u{7}"),
            "séquence d'ouverture absente : {screen:?}"
        );
        assert!(
            screen.contains("\u{1b}]8;;\u{7}"),
            "séquence de fermeture absente : {screen:?}"
        );
        // The visible text is unchanged: OSC 8 carries no width.
        assert!(screen.contains("voir "), "{screen:?}");
    }

    /// A destination comes from model output. An escape inside it would close
    /// the sequence early and run the rest as terminal commands.
    #[test]
    fn a_destination_carrying_an_escape_is_not_emitted() {
        let mut terminal = terminal(40, 6, Rect::new(0, 0, 40, 6));
        let mut line = HyperlinkLine::new(Line::raw("lien"));
        line.hyperlinks.push(TerminalHyperlink {
            columns: 0..4,
            destination: "https://a\u{1b}]0;pwned\u{7}".into(),
        });
        let insert = PendingHistoryInsert::inline_scrollback_hyperlink_lines([line]);

        HistoryInserter::new(InsertHistoryMode::InlineScrollback)
            .insert(&mut terminal, &insert)
            .expect("insertion inline");

        let screen = dump(terminal.backend().buffer());
        assert!(!screen.contains("\u{1b}]0;"), "injection émise : {screen:?}");
        assert!(screen.contains("lien"), "le texte reste rendu : {screen:?}");
    }

    /// A row that wraps spans several screen rows, so its columns no longer
    /// describe any one of them. It keeps its text and drops its links rather
    /// than marking the wrong span.
    #[test]
    fn a_wrapped_row_keeps_its_text_and_drops_its_links() {
        let mut terminal = terminal(12, 8, Rect::new(0, 0, 12, 8));
        let insert = PendingHistoryInsert::inline_scrollback([
            "un texte bien plus long que douze colonnes https://example.com",
        ]);

        HistoryInserter::new(InsertHistoryMode::InlineScrollback)
            .insert(&mut terminal, &insert)
            .expect("insertion inline");

        let screen = dump(terminal.backend().buffer());
        assert!(!screen.contains("\u{1b}]8;;"), "{screen:?}");
        assert!(screen.contains("colonnes"), "{screen:?}");
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
