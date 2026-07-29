//! Rendering view-models of the tools (US-035). Turns a tool call (name +
//! JSON `input`) and its result into a `Verb(target)` label and a secondary summary
//! (`⎿`). This is the Rust equivalent of Claude Code's `renderToolUseMessage` /
//! `renderToolResultMessage`: `render.rs` does not know the
//! tools, it delegates here. Pure and testable; no chrome color decision
//! (only the numbers are highlighted).
//!
//! The summaries are derived from the call `input` and the result `content`:
//! no change to the `agent-core` / `agent-tools` contracts (US-033).

use ratatui::style::Modifier;
use ratatui::text::Span;
use serde_json::Value;

use crate::measure;
use crate::render::sanitize;
use crate::theme::Theme;

/// Label of a tool call: an action verb + an optional target, rendered as
/// `Verb(target)`. Verb in plain English, like the reference harness.
pub struct Label {
    pub verb: String,
    pub target: Option<String>,
}

/// Verb + target displayed for a tool call, derived from the name and the input. An
/// unknown tool falls back on its raw name as the verb + a best-effort target
/// (US-035: never a panic on an unrecognized tool).
pub fn label(name: &str, input: &Value) -> Label {
    let verb = match name {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Update",
        "glob" => "List",
        "grep" => "Search",
        "bash" => "Run",
        other => other,
    }
    .to_string();

    let target = match name {
        "bash" => str_field(input, "command").map(|s| first_line_trunc(&s, 64)),
        "grep" => str_field(input, "pattern").map(|s| trunc(&s, 56)),
        _ => str_field(input, "path")
            .or_else(|| str_field(input, "pattern").map(|s| trunc(&s, 56)))
            .or_else(|| str_field(input, "command").map(|s| first_line_trunc(&s, 64))),
    };

    Label { verb, target }
}

/// Secondary summary (`⎿`) of a SUCCESSFUL tool result, as spans (numbers
/// are highlighted). `call` = `(name, input)` paired by id, or `None` when the
/// result is orphan (US-033: degraded generic display, without a panic).
pub fn result_summary(
    call: Option<(&str, &Value)>,
    content: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let dim = theme.dim();
    let num = theme.fg().add_modifier(Modifier::BOLD);

    let Some((name, input)) = call else {
        return vec![Span::styled(first_line_trunc(&sanitize(content), 80), dim)];
    };

    match name {
        // Lines read = numbered lines (`{lineno}\t{line}` format of the read tool).
        "read" => count(
            "Read ",
            content.lines().filter(|l| l.contains('\t')).count(),
            "line",
            "lines",
            dim,
            num,
        ),
        "write" => count(
            "Wrote ",
            line_count(&str_field(input, "content").unwrap_or_default()),
            "line",
            "lines",
            dim,
            num,
        ),
        // EP-010 approximation (without a diff library): lines of the new vs old
        // text. The exact count (real diff) comes in EP-011 (US-038).
        "edit" => {
            let added = line_count(&str_field(input, "new_string").unwrap_or_default());
            let removed = line_count(&str_field(input, "old_string").unwrap_or_default());
            let mut s = vec![
                Span::styled("Added ", dim),
                Span::styled(added.to_string(), num),
            ];
            s.push(Span::styled(unit(added, " line", " lines"), dim));
            s.push(Span::styled(", removed ", dim));
            s.push(Span::styled(removed.to_string(), num));
            s.push(Span::styled(unit(removed, " line", " lines"), dim));
            s
        }
        // US-019 AC2: a Code Mode cell reports its STATE, not the first line of
        // whatever the script printed. `yielded` is the one a model must react
        // to, and a user reading a transcript needs the same fact.
        "exec" | "wait" if cell_state_line(content).is_some() => match cell_state_line(content) {
            Some(state) => vec![Span::styled(state, dim)],
            None => vec![Span::styled(first_line_trunc(&sanitize(content), 80), dim)],
        },
        "glob" => count("Found ", listed(content), "file", "files", dim, num),
        "grep" => count("Found ", listed(content), "match", "matches", dim, num),
        "bash" => {
            let n = content.lines().filter(|l| !l.trim().is_empty()).count();
            if n == 0 {
                vec![Span::styled("Ran (no output)", dim)]
            } else {
                count("Ran · ", n, "line", "lines", dim, num)
            }
        }
        _ => vec![Span::styled(first_line_trunc(&sanitize(content), 80), dim)],
    }
}

/// State line a Code Mode cell result always ends with (`exec` / `wait`),
/// or `None` for a result that carries none.
///
/// Anchored on the two sentences `agent-tools::code_mode` appends last, the same
/// way [`is_user_rejection`] is anchored on the Registry messages: the TUI must
/// not depend on `agent-code-mode` to stay a rendering crate, and the state is
/// exactly what a transcript would otherwise lose. Read from the END because the
/// script's own output comes first and can say anything.
pub fn cell_state_line(content: &str) -> Option<String> {
    cell_state_split(content).map(|(state, _)| state)
}

/// The state line and everything else, for a surface that shows the two
/// separately. Without the split, a failed cell prints its state twice: once as
/// the state and once as the "first line" of its own error.
pub fn cell_state_split(content: &str) -> Option<(String, String)> {
    let clean = sanitize(content);
    let state = clean
        .lines()
        .map(str::trim)
        .rfind(|line| is_cell_state(line))?
        .to_string();
    let rest = clean
        .lines()
        .filter(|line| !is_cell_state(line.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    Some((trunc(&state, 100), rest))
}

fn is_cell_state(line: &str) -> bool {
    line.starts_with("Script running with cell ID ") || line.starts_with("Cell ")
}

/// Error message of a tool result, on one line prefixed with `Error:`, ANSI
/// stripped (US-036: no ANSI residue coming from a colored tool output).
pub fn error_summary(content: &str) -> String {
    let clean = sanitize(content);
    let first = clean
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(failure without message)");
    let first = trunc(first, 120);
    if first.starts_with("Error") || first.starts_with("error") {
        first
    } else {
        format!("Error: {first}")
    }
}

/// Number of non-empty lines BEYOND the first (indicator `... +N lines`
/// when a multi-line error is bounded to its 1st line, US-036).
pub fn extra_lines(content: &str) -> usize {
    sanitize(content)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .count()
        .saturating_sub(1)
}

/// Does the content of an `is_error` result correspond to a permission REJECTION
/// (user refusal or refusal by the mode) rather than a real tool error?
/// The distinction drives the tint: muted (deliberate rejection) vs red (US-036).
/// ANCHORED on the two Registry messages (`registry.rs`: "permission refusée
/// pour ..." / "action « ... » refusée par l'utilisateur") rather than a floating
/// "refusée" substring: a real tool error quoting that word (e.g. a bash output
/// saying "connexion refusée") must not be taken for a deliberate refusal.
pub fn is_user_rejection(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("permission denied for") || t.starts_with("action \"")
}

/// Label of a rejection (muted tone): 1st non-empty line, ANSI stripped.
pub fn reject_summary(content: &str) -> String {
    let clean = sanitize(content);
    let first = clean
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("action rejected");
    trunc(first, 120)
}

// ── Pure helpers ────────────────────────────────────────────────────────────────

fn str_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Number of displayable lines of a listing (excludes the `…` truncation footer).
fn listed(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('…'))
        .count()
}

/// Number of lines of a text (`""` -> 0).
fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

fn unit(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 { singular } else { plural }.to_string()
}

/// `{prefix}{n} {unit}` with the number highlighted.
fn count(
    prefix: &str,
    n: usize,
    singular: &str,
    plural: &str,
    dim: ratatui::style::Style,
    num: ratatui::style::Style,
) -> Vec<Span<'static>> {
    vec![
        Span::styled(prefix.to_string(), dim),
        Span::styled(n.to_string(), num),
        Span::styled(format!(" {}", if n == 1 { singular } else { plural }), dim),
    ]
}

/// Truncates to `max` columns (char-aware, `…` ellipsis).
fn trunc(s: &str, max: usize) -> String {
    measure::truncate(s, max)
}

/// 1st non-empty line, truncated (for bash / a multi-line command).
fn first_line_trunc(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    trunc(line, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn label_maps_known_verbs_and_target() {
        let l = label(
            "edit",
            &json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"}),
        );
        assert_eq!(l.verb, "Update");
        assert_eq!(l.target.as_deref(), Some("src/main.rs"));
        let b = label("bash", &json!({"command": "cargo test\nsecond line"}));
        assert_eq!(b.verb, "Run");
        assert_eq!(b.target.as_deref(), Some("cargo test"));
    }

    #[test]
    fn label_unknown_tool_falls_back_to_name() {
        let l = label("mcp__srv__do", &json!({"x": 1}));
        assert_eq!(l.verb, "mcp__srv__do");
        assert!(l.target.is_none(), "pas de panic, cible best-effort vide");
    }

    #[test]
    fn edit_summary_counts_added_and_removed() {
        let theme = Theme::new(false);
        let spans = result_summary(
            Some((
                "edit",
                &json!({"old_string": "x\ny", "new_string": "a\nb\nc"}),
            )),
            "Edited: f (level 1)",
            &theme,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Added 3 lines, removed 2 lines");
    }

    #[test]
    fn write_summary_singular_plural() {
        let theme = Theme::new(false);
        let one = result_summary(Some(("write", &json!({"content": "seule"}))), "", &theme);
        assert_eq!(
            one.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "Wrote 1 line"
        );
    }

    #[test]
    fn read_summary_counts_numbered_lines() {
        let theme = Theme::new(false);
        let content = "     1\tfn main() {\n     2\t}\n(fin)";
        let spans = result_summary(Some(("read", &json!({"path": "a.rs"}))), content, &theme);
        assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "Read 2 lines"
        );
    }

    /// US-019 AC2: the summary of a Code Mode result is the cell STATE, not the
    /// first line the script happened to print. `yielded` is what tells a reader
    /// the work is still running.
    #[test]
    fn a_code_mode_result_summarizes_the_cell_state() {
        let theme = Theme::new(false);
        let input = json!({ "input": "text(1);" });
        let yielded = result_summary(
            Some(("exec", &input)),
            "42\nScript running with cell ID cell_1. Call `wait` with this cell_id to resume.",
            &theme,
        );
        assert_eq!(
            yielded
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "Script running with cell ID cell_1. Call `wait` with this cell_id to resume."
        );

        let completed =
            result_summary(Some(("wait", &input)), "42\nCell cell_1 completed.", &theme);
        assert_eq!(
            completed
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "Cell cell_1 completed."
        );
    }

    /// The split is what keeps a failed cell from printing its state twice: once
    /// as the state, once as the first line of its own error.
    #[test]
    fn the_cell_state_is_separated_from_the_rest_of_the_result() {
        let (state, rest) =
            cell_state_split("boom happened\nCell cell_2 failed.\nscript_error: Error: boom")
                .expect("a cell result carries its state");
        assert_eq!(state, "Cell cell_2 failed.");
        assert_eq!(rest, "boom happened\nscript_error: Error: boom");
        assert!(!rest.contains("Cell cell_2 failed."));

        // A result that is not a cell has no state to lift out.
        assert!(cell_state_split("Compiling agent-core\nFinished").is_none());
    }

    #[test]
    fn orphan_result_degrades_without_panic() {
        let theme = Theme::new(false);
        let spans = result_summary(None, "some output\nnext", &theme);
        assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "some output"
        );
    }

    #[test]
    fn error_summary_prefixes_once() {
        assert_eq!(error_summary("anchor not found"), "Error: anchor not found");
        assert_eq!(
            error_summary("Error: already prefixed"),
            "Error: already prefixed"
        );
        assert_eq!(error_summary("   \n  real message"), "Error: real message");
    }

    #[test]
    fn error_summary_strips_ansi() {
        let out = error_summary("\u{1b}[31mRed error\u{1b}[0m");
        assert_eq!(out, "Error: Red error");
        assert!(!out.contains('\u{1b}'), "ANSI residue: {out:?}");
    }

    #[test]
    fn extra_lines_counts_beyond_first() {
        assert_eq!(extra_lines("single"), 0);
        assert_eq!(extra_lines("a\nb\nc"), 2);
        assert_eq!(extra_lines("a\n\n  \nb"), 1);
    }

    #[test]
    fn rejection_detected_from_registry_messages() {
        assert!(is_user_rejection("action \"edit\" rejected by user"));
        assert!(is_user_rejection(
            "permission denied for \"bash\" (mode Plan)"
        ));
        assert!(!is_user_rejection("anchor not found"));
        assert!(!is_user_rejection("curl: (7) connection refused by host"));
        assert!(!is_user_rejection(
            "the connection was refused by the server"
        ));
    }
}
