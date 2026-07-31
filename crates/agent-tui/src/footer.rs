//! Footer under the composer, adapted from Codex `tui/src/bottom_pane/footer.rs`.
//!
//! The footer is PURE rendering: it turns [`FooterProps`] into lines and never
//! mutates state. It does not decide WHICH content to show either; that comes
//! from the [`FooterMode`] the caller resolves from `AppState`.
//!
//! Vocabulary borrowed from Codex:
//! - "status line" is the ambient contextual row (model, workspace). It occupies
//!   the left half whenever the footer has nothing more urgent to say.
//! - "instructional footer" is a row telling the user what to do NEXT (quit
//!   confirmation, shortcut overlay). It EVICTS the status line, because an
//!   instruction that scrolls away under ambient context is useless.
//!
//! Collapse rule, in order: the right indicator is dropped before the left
//! status line is truncated, and the left is truncated with `…` rather than
//! wrapping. The footer is always exactly one line except in
//! [`FooterMode::ShortcutOverlay`].

use crate::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::measure;
use crate::theme::Theme;

/// Left and right margin of the footer, in columns. Codex reserves the same two
/// columns for the composer gutter, so the footer text aligns with the input
/// text rather than with the `›` glyph.
pub(crate) const FOOTER_INDENT_COLS: u16 = 2;
/// Minimum gap between the left status line and the right indicator.
const FOOTER_GAP_COLS: u16 = 1;
const SEPARATOR: &str = " · ";

/// Selects which footer content is rendered.
///
/// The base mode comes solely from whether the composer holds a draft;
/// transient modes override it while their condition lasts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FooterMode {
    /// Ambient row: the status line owns the footer.
    #[default]
    ComposerEmpty,
    /// Same row, but the shortcut affordance is suppressed while typing.
    ComposerHasDraft,
    /// Transient "press again to quit" reminder (Ctrl+C).
    QuitShortcutReminder,
    /// Multi-line shortcut cheatsheet, toggled with `?` on an empty composer.
    ShortcutOverlay,
}

impl FooterMode {
    /// Does this mode spend the row on ambient context rather than on an
    /// instruction?
    fn is_passive(self) -> bool {
        matches!(self, Self::ComposerEmpty | Self::ComposerHasDraft)
    }
}

/// Rendering inputs of the footer.
///
/// The caller builds these from `AppState`; the footer treats them as
/// authoritative and infers nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FooterProps {
    pub mode: FooterMode,
    /// Status line segments, already formatted, joined with ` · `.
    pub status_line: Vec<StatusSegment>,
    /// Right indicator, shown only when the mode is passive. Empty = hidden.
    pub mode_indicator: Option<String>,
    /// A turn is running: Ctrl+C interrupts instead of arming the quit.
    pub is_task_running: bool,
}

/// One status line segment plus the accent that colors it.
///
/// Codex colors each item by category (model, path, branch). Pyxis keeps its
/// monochrome-plus-one-accent palette, so the categories collapse onto the two
/// tints the theme already reserves for meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusSegment {
    pub text: String,
    pub accent: StatusAccent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusAccent {
    /// Model identity.
    Model,
    /// Filesystem path.
    Path,
}

impl StatusSegment {
    pub fn model(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            accent: StatusAccent::Model,
        }
    }

    pub fn path(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            accent: StatusAccent::Path,
        }
    }

    fn style(&self, theme: &Theme) -> ratatui::style::Style {
        match self.accent {
            StatusAccent::Model => theme.accent(),
            StatusAccent::Path => theme.success(),
        }
    }
}

/// Height of the footer content, spacing excluded.
pub(crate) fn height(props: &FooterProps, width: u16) -> u16 {
    match props.mode {
        FooterMode::ShortcutOverlay => overlay_row_count(width),
        _ => 1,
    }
}

/// Renders the footer into `area`, whose height must come from [`height`].
pub(crate) fn render(frame: &mut Frame, area: Rect, props: &FooterProps, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if props.mode == FooterMode::ShortcutOverlay {
        let lines = overlay_lines(area.width, theme);
        frame.render_widget(Paragraph::new(indent(lines)), area);
        return;
    }

    let left = left_line(props, theme);
    let right = props
        .mode
        .is_passive()
        .then_some(props.mode_indicator.as_ref())
        .flatten()
        .map(|label| Line::from(Span::styled(label.clone(), theme.dim())));

    // The right indicator is ambient; it goes first when the row gets tight.
    let inner = area.width.saturating_sub(FOOTER_INDENT_COLS * 2);
    let left_width = line_width(&left);
    let right_width = right.as_ref().map(line_width).unwrap_or(0);
    let show_right = right_width > 0
        && left_width
            .saturating_add(FOOTER_GAP_COLS)
            .saturating_add(right_width)
            <= inner;

    let budget = if show_right {
        inner.saturating_sub(right_width + FOOTER_GAP_COLS)
    } else {
        inner
    };
    frame.render_widget(
        Paragraph::new(indent(vec![truncate(left, budget)])),
        Rect { height: 1, ..area },
    );

    if show_right && let Some(right) = right {
        let x = area
            .x
            .saturating_add(area.width)
            .saturating_sub(FOOTER_INDENT_COLS)
            .saturating_sub(right_width);
        frame.render_widget(
            Paragraph::new(right),
            Rect {
                x,
                y: area.y,
                width: right_width,
                height: 1,
            },
        );
    }
}

/// Left half: the instruction when the mode has one, the status line otherwise.
fn left_line(props: &FooterProps, theme: &Theme) -> Line<'static> {
    match props.mode {
        FooterMode::QuitShortcutReminder => Line::from(vec![
            Span::styled("ctrl + c", theme.dim()),
            Span::styled(" again to quit", theme.faint()),
        ]),
        FooterMode::ShortcutOverlay => Line::default(),
        FooterMode::ComposerEmpty | FooterMode::ComposerHasDraft => {
            if props.status_line.is_empty() {
                // No ambient context to show: fall back on the affordance that
                // makes the overlay discoverable at all.
                return Line::from(vec![
                    Span::styled("?", theme.dim()),
                    Span::styled(" for shortcuts", theme.faint()),
                ]);
            }
            let mut spans = Vec::with_capacity(props.status_line.len() * 2);
            for segment in &props.status_line {
                if !spans.is_empty() {
                    spans.push(Span::styled(SEPARATOR, theme.faint()));
                }
                spans.push(Span::styled(segment.text.clone(), segment.style(theme)));
            }
            Line::from(spans)
        }
    }
}

/// Shortcut cheatsheet, laid out in two columns like Codex.
///
/// Only shortcuts Pyxis actually binds appear here: an overlay that advertises
/// a key doing nothing is worse than no overlay.
const SHORTCUTS: &[(&str, &str)] = &[
    ("/", "for commands"),
    ("@", "for file paths"),
    ("ctrl + j", "for newline"),
    ("enter", "to send"),
    ("tab", "to complete"),
    ("↑ ↓", "to browse history"),
    ("ctrl + a", "to go to line start"),
    ("ctrl + e", "to go to line end"),
    ("ctrl + w", "to delete previous word"),
    ("ctrl + u", "to clear the input"),
    ("ctrl + t", "to view transcript"),
    ("ctrl + c", "to interrupt or exit"),
];
const OVERLAY_COLUMNS: usize = 2;
const OVERLAY_COLUMN_GAP: usize = 4;
/// Below this inner width the two columns no longer breathe: stack them.
const OVERLAY_TWO_COLUMN_MIN_WIDTH: u16 = 56;

fn overlay_columns(width: u16) -> usize {
    if width.saturating_sub(FOOTER_INDENT_COLS * 2) >= OVERLAY_TWO_COLUMN_MIN_WIDTH {
        OVERLAY_COLUMNS
    } else {
        1
    }
}

fn overlay_row_count(width: u16) -> u16 {
    // +2: a blank spacer and the closing hint.
    (SHORTCUTS.len().div_ceil(overlay_columns(width)) + 2) as u16
}

fn overlay_lines(width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let columns = overlay_columns(width);
    // Keys stay LEFT-aligned like Codex: padding them into a right-aligned
    // column reads as a table, and the eye then scans the gap instead of the key.
    let entry_width = |(key, label): &(&str, &str)| measure::width(key) + 1 + measure::width(label);
    let column_width = SHORTCUTS.iter().map(entry_width).max().unwrap_or(0);

    let mut lines: Vec<Line<'static>> = SHORTCUTS
        .chunks(columns)
        .map(|chunk| {
            let mut spans = Vec::with_capacity(chunk.len() * 3);
            for (idx, entry) in chunk.iter().enumerate() {
                let (key, label) = entry;
                spans.push(Span::styled(format!("{key} "), theme.dim()));
                spans.push(Span::styled((*label).to_string(), theme.faint()));
                if idx + 1 < chunk.len() {
                    let pad = column_width.saturating_sub(entry_width(entry)) + OVERLAY_COLUMN_GAP;
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("?", theme.dim()),
        Span::styled(" or ", theme.faint()),
        Span::styled("esc", theme.dim()),
        Span::styled(" to close", theme.faint()),
    ]));
    lines
}

fn indent(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let pad = " ".repeat(FOOTER_INDENT_COLS as usize);
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn line_width(line: &Line<'static>) -> u16 {
    line.spans
        .iter()
        .map(|span| measure::width(span.content.as_ref()))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

/// Clips `line` to `budget` columns, marking the cut with `…`.
///
/// Truncating span by span (rather than on the concatenated text) keeps each
/// segment's style, so a cut status line stays readable instead of collapsing
/// to one tint.
fn truncate(line: Line<'static>, budget: u16) -> Line<'static> {
    if line_width(&line) <= budget {
        return line;
    }
    if budget == 0 {
        return Line::default();
    }
    let budget = budget as usize;
    let mut used = 0usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let span_width = measure::width(span.content.as_ref());
        // The strict comparison keeps a column for the ellipsis that always
        // terminates the line.
        if used + span_width < budget {
            used += span_width;
            spans.push(span);
            continue;
        }
        let room = budget - 1 - used;
        if room > 0 {
            let mut kept = String::new();
            let mut kept_width = 0usize;
            for grapheme in
                unicode_segmentation::UnicodeSegmentation::graphemes(span.content.as_ref(), true)
            {
                let w = measure::width(grapheme);
                if kept_width + w > room {
                    break;
                }
                kept_width += w;
                kept.push_str(grapheme);
            }
            if !kept.is_empty() {
                spans.push(Span::styled(kept, span.style));
            }
        }
        break;
    }
    spans.push(Span::raw("…"));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_terminal::Terminal;
    use ratatui::backend::TestBackend;

    fn draw(props: &FooterProps, width: u16, height_rows: u16) -> Vec<String> {
        let mut term = Terminal::full_screen(TestBackend::new(width, height_rows), width, height_rows);
        term.draw(|frame| {
            let theme = Theme::new(true);
            let area = frame.area();
            render(frame, area, props, &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..height_rows)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn status_props() -> FooterProps {
        FooterProps {
            mode: FooterMode::ComposerEmpty,
            status_line: vec![
                StatusSegment::model("gpt-5 high"),
                StatusSegment::path("~/dev/pyxis"),
            ],
            mode_indicator: None,
            is_task_running: false,
        }
    }

    #[test]
    fn status_line_is_indented_and_joined_with_middots() {
        let rows = draw(&status_props(), 60, 1);
        assert_eq!(rows[0], "  gpt-5 high · ~/dev/pyxis");
    }

    #[test]
    fn mode_indicator_is_right_aligned_with_a_two_column_margin() {
        let mut props = status_props();
        props.mode_indicator = Some("Read Only".into());
        let rows = draw(&props, 60, 1);
        assert!(rows[0].starts_with("  gpt-5 high · ~/dev/pyxis"));
        assert!(
            rows[0].ends_with("Read Only"),
            "indicator should touch the right margin: {rows:?}"
        );
        assert_eq!(
            measure::width(&rows[0]),
            60 - FOOTER_INDENT_COLS as usize,
            "two columns must stay free on the right"
        );
    }

    #[test]
    fn indicator_is_dropped_before_the_status_line_is_truncated() {
        let mut props = status_props();
        props.mode_indicator = Some("Read Only".into());
        let rows = draw(&props, 34, 1);
        assert_eq!(rows[0], "  gpt-5 high · ~/dev/pyxis");
    }

    #[test]
    fn status_line_is_truncated_with_an_ellipsis_when_the_row_is_too_narrow() {
        let rows = draw(&status_props(), 16, 1);
        // 2 columns of indent + the 12 that remain once both margins are paid.
        assert_eq!(measure::width(&rows[0]), 14);
        assert!(rows[0].ends_with('…'), "{rows:?}");
    }

    #[test]
    fn quit_reminder_evicts_the_status_line_and_the_indicator() {
        let mut props = status_props();
        props.mode = FooterMode::QuitShortcutReminder;
        props.mode_indicator = Some("Read Only".into());
        let rows = draw(&props, 60, 1);
        assert_eq!(rows[0], "  ctrl + c again to quit");
    }

    #[test]
    fn empty_status_line_falls_back_on_the_shortcut_affordance() {
        let mut props = status_props();
        props.status_line.clear();
        let rows = draw(&props, 60, 1);
        assert_eq!(rows[0], "  ? for shortcuts");
    }

    #[test]
    fn overlay_lists_every_bound_shortcut_and_how_to_close_it() {
        let mut props = status_props();
        props.mode = FooterMode::ShortcutOverlay;
        let rows_needed = height(&props, 80);
        let rows = draw(&props, 80, rows_needed);
        let text = rows.join("\n");
        for (key, label) in SHORTCUTS {
            assert!(text.contains(key), "missing key {key}:\n{text}");
            assert!(text.contains(label), "missing label {label}:\n{text}");
        }
        assert!(text.contains("to close"), "{text}");
        assert_eq!(rows_needed as usize, SHORTCUTS.len() / 2 + 2);
    }

    #[test]
    fn overlay_stacks_into_one_column_on_a_narrow_terminal() {
        let mut props = status_props();
        props.mode = FooterMode::ShortcutOverlay;
        let rows_needed = height(&props, 40);
        assert_eq!(rows_needed as usize, SHORTCUTS.len() + 2);
        let rows = draw(&props, 40, rows_needed);
        for row in &rows {
            assert!(
                measure::width(row) <= 40,
                "overlay row overflows a 40-column terminal: {row:?}"
            );
        }
    }

    #[test]
    fn narrow_terminal_never_overflows_the_row() {
        let mut props = status_props();
        props.mode_indicator = Some("Read Only".into());
        for width in [1u16, 2, 3, 8, 20, 33, 80] {
            let rows = draw(&props, width, 1);
            assert!(
                measure::width(&rows[0]) <= width as usize,
                "width {width} overflows: {rows:?}"
            );
        }
    }
}
