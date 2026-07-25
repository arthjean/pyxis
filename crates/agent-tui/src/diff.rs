//! Structured diff computation for rendering (US-037). Derives a diff ready to style
//! from the `input` of a mutating tool: `edit` (old_string -> new_string, real
//! inter-line diff + intra-line emphasis through `similar`) or `write` (content = additions,
//! bounded preview). ALSO serves the permission dialog preview (US-039), where bash and
//! unknown tools are represented as context lines.
//!
//! Pure and BOUNDED (never any I/O, never a file read back): rendering (`render.rs`)
//! alone applies the colors. Line numbers RELATIVE to the input fragments
//! (not the absolute file numbers, which we do not have without reading the disk).

use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// Intra-line segment (word-diff): a text fragment and whether it is
/// emphasized (the actually changed portion of an added/removed line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub emphasized: bool,
}

/// One row of the structured diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Added line (`+`), with word-by-word segments.
    Add {
        lineno: Option<usize>,
        segs: Vec<Seg>,
    },
    /// Removed line (`-`), with word-by-word segments.
    Remove {
        lineno: Option<usize>,
        segs: Vec<Seg>,
    },
    /// Context line (unchanged) or note (bash/unknown).
    Context { lineno: Option<usize>, text: String },
    /// Separation between two non-contiguous hunks.
    Gap,
    /// `N` more lines not displayed (past the bound).
    Truncated(usize),
}

impl Row {
    /// Line number carried by the row (to calibrate the gutter).
    pub fn lineno(&self) -> Option<usize> {
        match self {
            Row::Add { lineno, .. } | Row::Remove { lineno, .. } | Row::Context { lineno, .. } => {
                *lineno
            }
            Row::Gap | Row::Truncated(_) => None,
        }
    }
}

/// Structured diff, ready to be styled by the rendering.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub rows: Vec<Row>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Context lines without a diff (bash/unknown preview for the permission, US-039).
pub fn note<I, S>(lines: I) -> Diff
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let rows = lines
        .into_iter()
        // Sanitized: the bash/unknown preview can quote an adversarial command/output.
        .map(|l| Row::Context {
            lineno: None,
            text: crate::render::sanitize(&l.into()),
        })
        .collect();
    Diff { rows }
}

/// Number of context lines around a change (like `diff -U3`).
const CONTEXT: usize = 3;
/// Hard bound of displayed lines (large edit/write); past it -> `Truncated`.
const MAX_ROWS: usize = 200;
/// Cost guard BEFORE the diff (US-037 AC4): the Myers of `similar` is O(N*D) and
/// `bound()` only truncates the OUTPUT. Past these thresholds on the input, we do
/// not diff (a giant `old_string`/`new_string` crafted by the model would blow up
/// the cost before any bound).
const MAX_DIFF_LINES: usize = 4000;
const MAX_DIFF_BYTES: usize = 512 * 1024;

/// Builds the diff of a mutating tool call from its `input`. `None` when
/// the tool is not mutating or the input is unusable (US-038: no diff).
pub fn from_tool(name: &str, input: &Value) -> Option<Diff> {
    // The `input` comes from the (adversarial) model and is rendered as is by `push_diff`
    // WITHOUT going through `sanitize` again (the diff does not travel the markdown path).
    // So we sanitize HERE, a single choke point, BEFORE diffing: a crafted `new_string`
    // carrying OSC/CSI must never reach the terminal intact.
    use crate::render::sanitize;
    match name {
        "edit" => {
            let old_raw = input.get("old_string")?.as_str()?;
            let new_raw = input.get("new_string")?.as_str()?;
            if raw_too_large(old_raw) || raw_too_large(new_raw) {
                return Some(too_large_note(old_raw, new_raw));
            }
            let old = sanitize(old_raw);
            let new = sanitize(new_raw);
            let d = from_edit(&old, &new);
            (!d.is_empty()).then_some(d)
        }
        "write" => {
            let raw = input.get("content")?.as_str()?;
            if raw_too_large(raw) {
                return Some(note([format!(
                    "(write too large to preview: at least {} lines)",
                    bounded_line_count(raw)
                )]));
            }
            let content = sanitize(raw);
            let d = from_write(&content);
            (!d.is_empty()).then_some(d)
        }
        _ => None,
    }
}

fn raw_too_large(s: &str) -> bool {
    s.len() > MAX_DIFF_BYTES || s.lines().take(MAX_DIFF_LINES + 1).count() > MAX_DIFF_LINES
}

fn bounded_line_count(s: &str) -> usize {
    s.lines().take(MAX_DIFF_LINES + 1).count()
}

fn too_large_note(old: &str, new: &str) -> Diff {
    note([format!(
        "(diff too large to preview: at least {} -> {} lines)",
        bounded_line_count(old),
        bounded_line_count(new)
    )])
}

/// Real inter-line diff old -> new, with intra-line emphasis (word-diff).
fn from_edit(old: &str, new: &str) -> Diff {
    // Cost guard (US-037 AC4): bound BEFORE diffing. The byte threshold also catches
    // the pathological case of ONE giant line (where the O(L^2) intra-line diff
    // would be expensive without crossing the line threshold).
    if old.len() > MAX_DIFF_BYTES
        || new.len() > MAX_DIFF_BYTES
        || old.lines().count() > MAX_DIFF_LINES
        || new.lines().count() > MAX_DIFF_LINES
    {
        return note([format!(
            "(diff too large to preview: {} -> {} lines)",
            old.lines().count(),
            new.lines().count()
        )]);
    }
    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<Row> = Vec::new();
    for (gi, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if gi > 0 {
            rows.push(Row::Gap);
        }
        for op in group {
            // Numbers tracked manually from the op bounds (robust whatever
            // the index API of `InlineChange` is).
            let mut old_ln = op.old_range().start;
            let mut new_ln = op.new_range().start;
            for change in diff.iter_inline_changes(op) {
                let segs: Vec<Seg> = change
                    .iter_strings_lossy()
                    .map(|(emph, value)| Seg {
                        text: strip_eol(&value),
                        emphasized: emph,
                    })
                    .collect();
                match change.tag() {
                    ChangeTag::Equal => {
                        let text = segs.iter().map(|s| s.text.as_str()).collect::<String>();
                        rows.push(Row::Context {
                            lineno: Some(new_ln + 1),
                            text,
                        });
                        old_ln += 1;
                        new_ln += 1;
                    }
                    ChangeTag::Delete => {
                        rows.push(Row::Remove {
                            lineno: Some(old_ln + 1),
                            segs,
                        });
                        old_ln += 1;
                    }
                    ChangeTag::Insert => {
                        rows.push(Row::Add {
                            lineno: Some(new_ln + 1),
                            segs,
                        });
                        new_ln += 1;
                    }
                }
            }
        }
    }
    bound(rows)
}

/// Creation/replacement: all the written content presented as added lines,
/// bounded preview (we do not have the old content without reading the disk).
fn from_write(content: &str) -> Diff {
    let mut rows: Vec<Row> = Vec::new();
    let total = content.lines().count();
    for (i, line) in content.lines().enumerate() {
        if i >= MAX_ROWS {
            rows.push(Row::Truncated(total - i));
            break;
        }
        rows.push(Row::Add {
            lineno: Some(i + 1),
            segs: vec![Seg {
                text: line.to_string(),
                emphasized: false,
            }],
        });
    }
    Diff { rows }
}

/// Bounds the number of rows and adds a truncation marker.
fn bound(mut rows: Vec<Row>) -> Diff {
    if rows.len() > MAX_ROWS {
        let extra = rows.len() - MAX_ROWS;
        rows.truncate(MAX_ROWS);
        rows.push(Row::Truncated(extra));
    }
    Diff { rows }
}

/// Removes the line terminator of a segment (`similar` keeps the `\n`).
fn strip_eol(s: &str) -> String {
    s.trim_end_matches(['\n', '\r']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_produces_remove_then_add_with_word_emphasis() {
        let d = from_edit("let x = 1;\n", "let x = 2;\n");
        // One removal + one addition (intra-line replacement).
        let removes = d
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Remove { .. }))
            .count();
        let adds = d
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Add { .. }))
            .count();
        assert_eq!((removes, adds), (1, 1));
        // At least one emphasized segment (the `1`/`2` that changes), and not the WHOLE line.
        let segs = d
            .rows
            .iter()
            .find_map(|r| match r {
                Row::Add { segs, .. } => Some(segs),
                _ => None,
            })
            .expect("expected added line");
        assert!(
            segs.iter().any(|s| s.emphasized),
            "expected inline emphasis"
        );
        assert!(
            segs.iter().any(|s| !s.emphasized),
            "inline context remains neutral"
        );
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "let x = 2;");
        assert!(!joined.contains('\n'), "line terminator stripped");
    }

    #[test]
    fn edit_keeps_context_lines_with_numbers() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nC\nd\ne\n";
        let d = from_edit(old, new);
        // Line 3 changes; the context (lines 1,2,4,5) is kept and numbered.
        assert!(d.rows.iter().any(|r| matches!(
            r,
            Row::Context {
                lineno: Some(2),
                ..
            }
        )));
        assert!(d.rows.iter().any(|r| matches!(
            r,
            Row::Add {
                lineno: Some(3),
                ..
            }
        )));
    }

    #[test]
    fn no_op_edit_is_empty() {
        assert!(from_edit("same\n", "same\n").is_empty());
    }

    #[test]
    fn write_is_all_additions() {
        let d = from_write("line 1\nline 2\nline 3");
        assert_eq!(d.rows.len(), 3);
        assert!(d.rows.iter().all(|r| matches!(r, Row::Add { .. })));
        assert!(matches!(
            &d.rows[0],
            Row::Add {
                lineno: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn write_bounds_huge_content() {
        let content = (0..500)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = from_write(&content);
        assert_eq!(d.rows.len(), MAX_ROWS + 1);
        assert!(matches!(d.rows.last(), Some(Row::Truncated(n)) if *n == 500 - MAX_ROWS));
    }

    #[test]
    fn from_tool_dispatches_and_ignores_non_mutating() {
        assert!(from_tool("edit", &json!({"old_string": "a", "new_string": "b"})).is_some());
        assert!(from_tool("write", &json!({"content": "x"})).is_some());
        assert!(from_tool("read", &json!({"path": "a.rs"})).is_none());
        // degenerate input -> None, no panic.
        assert!(from_tool("edit", &json!({"path": "a.rs"})).is_none());
    }

    // Security: an adversarial `new_string`/`content` carrying OSC/CSI is sanitized
    // when the diff is built (the diff does not go through `sanitize` at render time).
    #[test]
    fn from_tool_sanitizes_adversarial_input() {
        let d = from_tool(
            "edit",
            &json!({
                "old_string": "let x = 1;",
                "new_string": "let x = 2;\x1b]0;pwned\x07\x1b[31m"
            }),
        )
        .expect("non-empty diff");
        let any_esc = d.rows.iter().any(|r| match r {
            Row::Add { segs, .. } | Row::Remove { segs, .. } => {
                segs.iter().any(|s| s.text.contains('\u{1b}'))
            }
            Row::Context { text, .. } => text.contains('\u{1b}'),
            _ => false,
        });
        assert!(!any_esc, "escape sequence not sanitized in diff");
        let w = from_tool("write", &json!({"content": "ok\x1b[2J"})).expect("diff");
        assert!(
            !matches!(&w.rows[0], Row::Add { segs, .. } if segs[0].text.contains('\u{1b}')),
            "write not sanitized"
        );
    }

    // US-037 AC4: the cost guard bounds `from_edit` on a giant input (the Myers
    // diff does not run) -> bounded `note` fallback, without a panic nor a cost blow-up.
    #[test]
    fn from_edit_bounds_huge_input() {
        let huge = "x\n".repeat(MAX_DIFF_LINES + 100);
        let d = from_tool("edit", &json!({"old_string": "a", "new_string": huge})).expect("diff");
        assert!(
            d.rows.len() <= 2 && d.rows.iter().all(|r| matches!(r, Row::Context { .. })),
            "bounded fallback expected, not a full diff"
        );
        // Same on a single giant line (byte threshold).
        let big_line = "y".repeat(MAX_DIFF_BYTES + 1);
        let d2 = from_tool("edit", &json!({"old_string": "a", "new_string": big_line})).expect("d");
        assert!(matches!(&d2.rows[0], Row::Context { .. }));
    }

    #[test]
    fn note_builds_context_rows() {
        let d = note(["rm -rf /tmp/x".to_string()]);
        assert_eq!(d.rows.len(), 1);
        assert!(
            matches!(&d.rows[0], Row::Context { lineno: None, text } if text == "rm -rf /tmp/x")
        );
    }
}
