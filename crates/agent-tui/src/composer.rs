//! Layout of the multi-line composer.
//!
//! The input model stays a flat `String` with a byte-offset cursor
//! (`AppState::input`): all the menu logic and the existing tests read
//! the input as a `&str`. Multi-line is therefore DERIVED here, at the rendering
//! width, rather than stored as a `Vec<String>`.
//!
//! Central invariant: the visual lines PARTITION the input. Concatenated
//! in order, with the `\n` separators reinserted, they give back the input byte for
//! byte. That invariant is what guarantees that no character is lost on
//! folding (AC1) and that the screen position of the cursor matches exactly its
//! logical position (AC4).

use std::iter::Peekable;
use std::str::Chars;

use unicode_segmentation::UnicodeSegmentation;

use crate::measure;

/// A contiguous segment of the input occupying one line on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisualRow {
    /// Start byte offset in the input (inclusive).
    pub start: usize,
    /// End byte offset in the input (exclusive).
    pub end: usize,
    /// First segment of a logical line (not a fold continuation).
    pub first_of_line: bool,
}

/// Result of the layout computation for a given width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub rows: Vec<VisualRow>,
    /// Index of the visual line carrying the cursor.
    pub cursor_row: usize,
    /// Cursor column in the text area (gutter excluded), in cells.
    pub cursor_col: usize,
}

/// Folds `input` at `width` columns and locates the cursor.
///
/// `width` is the width of the TEXT area (gutter already subtracted); a
/// zero width is raised to 1 so that the algorithm always makes progress.
pub(crate) fn layout(input: &str, cursor: usize, width: usize) -> Layout {
    let width = width.max(1);
    let mut rows: Vec<VisualRow> = Vec::new();

    let mut line_start = 0usize;
    for line in input.split('\n') {
        let line_end = line_start + line.len();
        let mut seg_start = line_start;
        loop {
            let seg_end = seg_start + wrap_point(&input[seg_start..line_end], width);
            rows.push(VisualRow {
                start: seg_start,
                end: seg_end,
                first_of_line: seg_start == line_start,
            });
            if seg_end >= line_end {
                break;
            }
            seg_start = seg_end;
        }
        // +1: the `\n` consumed by `split` belongs to no segment.
        line_start = line_end + 1;
    }

    let cursor = cursor.min(input.len());
    let (mut cursor_row, mut cursor_col) = (rows.len().saturating_sub(1), 0);
    for (idx, row) in rows.iter().enumerate() {
        if cursor < row.start || cursor > row.end {
            continue;
        }
        cursor_row = idx;
        cursor_col = measure::width(&input[row.start..cursor]);
        // Cursor at the end of a FOLDED segment: it shows at the start of the
        // continuation, where the next typed character will appear.
        if cursor == row.end && rows.get(idx + 1).is_some_and(|next| !next.first_of_line) {
            cursor_row = idx + 1;
            cursor_col = 0;
        }
        break;
    }
    // A segment that is exactly full puts the cursor one column past the edge
    // when there is no continuation (end of input): we bring it back inside the
    // area rather than draw outside the frame.
    cursor_col = cursor_col.min(width.saturating_sub(1));

    Layout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// First byte offset where to cut `s` so that it fits in `width` columns.
///
/// Returns `s.len()` when everything fits. Otherwise cuts after the last space
/// encountered (the space stays on the current line, so the partition is
/// preserved), or hard when no space offers a cut point. Always makes
/// progress of at least one grapheme: a width of 1 facing a wide character
/// cannot loop.
fn wrap_point(s: &str, width: usize) -> usize {
    let mut used = 0usize;
    let mut last_space_end: Option<usize> = None;
    for (i, g) in s.grapheme_indices(true) {
        let w = measure::width(g);
        if used + w > width {
            let hard = if i == 0 { g.len() } else { i };
            return match last_space_end {
                Some(sp) if sp > 0 && sp < hard => sp,
                _ => hard,
            };
        }
        used += w;
        if g == " " {
            last_space_end = Some(i + g.len());
        }
    }
    s.len()
}

/// First visible line so that `cursor_row` stays inside the window.
pub(crate) fn scroll_offset(cursor_row: usize, total_rows: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    if total_rows <= visible {
        return 0;
    }
    let max_offset = total_rows - visible;
    cursor_row
        .saturating_sub(visible - 1)
        .min(max_offset)
        .min(cursor_row)
}

/// Width of a pasted tabulation, converted into spaces.
const TAB_WIDTH: usize = 4;

/// Neutralizes pasted content before it enters the composer (US-011).
///
/// ANSI escape sequences are removed ENTIRELY (introducer and
/// parameters): removing only the `ESC` byte would leave `[31m` as visible
/// text. The other control characters are removed, except `\n`. `\r\n`
/// and a lone `\r` become a plain `\n`.
///
/// Tabulation is converted into spaces: `unicode-width` gives it a
/// zero width whereas the terminal expands it to the next tab stop. Left
/// as is, it would shift the whole frame, exactly like an ANSI
/// sequence, without any width measurement seeing it coming.
///
/// The neutralization applies to the STORED content, not only to the display:
/// the model has no use for a terminal control sequence, and the content
/// sent must be the one that was read back on screen.
pub(crate) fn sanitize_paste(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push(c),
            '\t' => out.push_str(&" ".repeat(TAB_WIDTH)),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Consumes the body of an escape sequence, `ESC` already read.
fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        // CSI: parameters then a final byte in 0x40..=0x7E.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // OSC / DCS / SOS / PM / APC: terminated by BEL or by ST (`ESC \`).
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    chars.next_if(|next| *next == '\\');
                    break;
                }
            }
        }
        // Two-character escape (`ESC c`, `ESC (B`, ...): already consumed.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The partition invariant: the segments glued back give the input.
    fn recompose(input: &str, layout: &Layout) -> String {
        let mut out = String::new();
        for (idx, row) in layout.rows.iter().enumerate() {
            if idx > 0 && row.first_of_line {
                out.push('\n');
            }
            out.push_str(&input[row.start..row.end]);
        }
        out
    }

    #[test]
    fn wrapping_loses_no_character() {
        for input in [
            "",
            "court",
            "un texte nettement plus long que la largeur donnee",
            "ligne1\nligne2\n\nligne4",
            "motbeaucouptroplongpourtenirsurunelignedonnee",
            "  espaces   multiples   preserves  ",
            "héllo wörld ✅ 漢字テスト",
        ] {
            for width in [1usize, 2, 3, 7, 20, 80] {
                let l = layout(input, 0, width);
                assert_eq!(recompose(input, &l), input, "input={input:?} width={width}");
            }
        }
    }

    #[test]
    fn no_visual_row_exceeds_width() {
        let input = "un texte assez long pour etre replie plusieurs fois de suite";
        for width in [1usize, 4, 9, 17] {
            let l = layout(input, 0, width);
            for row in &l.rows {
                assert!(
                    measure::width(&input[row.start..row.end]) <= width,
                    "segment {:?} depasse {width}",
                    &input[row.start..row.end]
                );
            }
        }
    }

    #[test]
    fn explicit_newlines_produce_one_row_each() {
        let input = "a\n\nb";
        let l = layout(input, 0, 80);
        assert_eq!(l.rows.len(), 3);
        assert!(l.rows.iter().all(|r| r.first_of_line));
    }

    #[test]
    fn cursor_maps_to_logical_position_on_wrapped_line() {
        let input = "aaaa bbbb cccc";
        // width 10 -> "aaaa bbbb" then "cccc".
        let l = layout(input, 0, 10);
        assert_eq!(l.rows.len(), 2);
        let at_c = layout(input, 10, 10);
        assert_eq!((at_c.cursor_row, at_c.cursor_col), (1, 0));
        let end = layout(input, input.len(), 10);
        assert_eq!((end.cursor_row, end.cursor_col), (1, 4));
    }

    #[test]
    fn cursor_at_end_of_logical_line_stays_on_that_line() {
        let input = "ab\ncd";
        let l = layout(input, 2, 80);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 2));
    }

    #[test]
    fn cursor_column_counts_terminal_cells_not_bytes() {
        let input = "漢字x";
        let l = layout(input, "漢字".len(), 80);
        assert_eq!(l.cursor_col, 4);
    }

    #[test]
    fn wide_grapheme_never_stalls_on_narrow_width() {
        let input = "漢漢漢";
        let l = layout(input, 0, 1);
        assert_eq!(l.rows.len(), 3);
        assert_eq!(recompose(input, &l), input);
    }

    #[test]
    fn scroll_keeps_cursor_row_visible() {
        assert_eq!(scroll_offset(0, 20, 5), 0);
        assert_eq!(scroll_offset(4, 20, 5), 0);
        assert_eq!(scroll_offset(5, 20, 5), 1);
        assert_eq!(scroll_offset(19, 20, 5), 15);
        assert_eq!(scroll_offset(3, 3, 5), 0);
    }

    #[test]
    fn sanitize_strips_full_ansi_sequences() {
        assert_eq!(sanitize_paste("\u{1b}[31mrouge\u{1b}[0m"), "rouge");
        assert_eq!(sanitize_paste("\u{1b}]0;titre\u{7}ok"), "ok");
        assert_eq!(sanitize_paste("\u{1b}]8;;http://x\u{1b}\\lien"), "lien");
        assert_eq!(sanitize_paste("a\u{1b}[2Jb"), "ab");
        assert_eq!(sanitize_paste("\u{1b}c reset"), " reset");
    }

    #[test]
    fn sanitize_keeps_newlines_expands_tabs_drops_other_controls() {
        assert_eq!(sanitize_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(sanitize_paste("a\tb"), "a    b");
        assert_eq!(sanitize_paste("a\u{0}b\u{7}c\u{9b}d"), "abcd");
        assert_eq!(sanitize_paste("héllo 漢字 ✅"), "héllo 漢字 ✅");
    }

    /// No character surviving the neutralization can move the terminal
    /// cursor: neither `ESC`, nor another control, nor a tabulation.
    #[test]
    fn sanitized_paste_contains_no_cursor_moving_character() {
        let hostile = "\u{1b}[2J\u{1b}[1;1Heffacé\ttab\u{8}\u{c}\u{1b}]0;titre\u{7}fin\r\nsuite";
        let clean = sanitize_paste(hostile);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
        assert_eq!(clean, "effacé    tabfin\nsuite");
    }
}
