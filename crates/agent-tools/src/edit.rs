//! `edit` tool: tolerant anchored replacement (US-025). `old_string` is located
//! by 4 successive passes: exact (substring, intra-line edit) -> `trim_end`
//! -> `trim` -> Unicode normalization (lines), to absorb the divergences that
//! GPT-5.x generates from memory (NBSP, typographic dashes/quotes). The FIRST
//! pass with a UNIQUE match wins; >= 2 matches -> ambiguous, 0 after the
//! 4 passes -> not found (explicit failure, NO mutation, edge case #11). The
//! replacement applies to the ORIGINAL lines (content outside the target intact).
//! Mutation confined to the workspace.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::path::{
    confine, ensure_existing_path_no_links, ensure_policy_allows_write, guard_write_target,
    replace_file_confined,
};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_EDIT_ANCHOR_BYTES, MAX_EDIT_FILE_BYTES, Tool, ToolCtx, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditInput {
    pub path: String,
    /// Text to replace. Must be unique in the file.
    pub old_string: String,
    pub new_string: String,
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    type Input = EditInput;

    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> String {
        "Replace one unique occurrence of text in a file. old_string must locate \
         a unique target (otherwise the edit fails without modifying anything). \
         Matching tolerates trailing-space differences and Unicode variants \
         (typographic dashes/quotes, NBSP). Parameters: path, old_string, new_string."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the workspace." },
                "old_string": { "type": "string", "description": "Text to replace (unique anchor)." },
                "new_string": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        EDIT_GUIDELINES
    }
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.old_string.is_empty() {
            return Err(ValidationError::new(
                "old_string is empty: cannot anchor the edit",
            ));
        }
        if input.old_string == input.new_string {
            return Err(ValidationError::new(
                "old_string == new_string: edit has no effect",
            ));
        }
        if input.old_string.len() > MAX_EDIT_ANCHOR_BYTES {
            return Err(ValidationError::new(format!(
                "old_string too large: {} bytes > {MAX_EDIT_ANCHOR_BYTES}",
                input.old_string.len()
            )));
        }
        if input.new_string.len() > MAX_EDIT_ANCHOR_BYTES {
            return Err(ValidationError::new(format!(
                "new_string too large: {} bytes > {MAX_EDIT_ANCHOR_BYTES}",
                input.new_string.len()
            )));
        }
        // US-013/US-001: read-only policy and deferred-execution zones refused
        // before the permission decision, hence liftable by no mode.
        guard_write_target(&ctx.sandbox, &ctx.workspace, &input.path)
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        ensure_policy_allows_write(&ctx.sandbox, &ctx.workspace, &path, &input.path)?;
        ensure_existing_path_no_links(&ctx.workspace, &path, &input.path)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        if meta.len() > MAX_EDIT_FILE_BYTES {
            return Err(ToolError::Rejected(format!(
                "{} is too large for edit: {} bytes > {MAX_EDIT_FILE_BYTES}",
                input.path,
                meta.len()
            )));
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;

        let (updated, level) = match locate(&content, &input.old_string) {
            Ok(Anchor::Substring) => (
                content.replacen(&input.old_string, &input.new_string, 1),
                MatchLevel::Exact,
            ),
            Ok(Anchor::Lines { start, len, level }) => (
                apply_line_window(&content, start, len, &input.new_string),
                level,
            ),
            Err(LocateError::Ambiguous { count }) => {
                return Err(ToolError::Rejected(format!(
                    "ambiguous anchor in {}: {} matches; add unique context \
                     around old_string",
                    input.path, count
                )));
            }
            Err(LocateError::NotFound) => {
                return Err(ToolError::Rejected(format!(
                    "anchor not found in {} after 4 passes (exact, trim_end, trim, \
                     Unicode); verify old_string or add context",
                    input.path
                )));
            }
        };

        replace_file_confined(&ctx.workspace, &path, &input.path, updated.as_bytes()).await?;
        Ok(ToolOutput::text(format!(
            "Edited: {} ({})",
            input.path,
            level.label()
        )))
    }
}

/// Behavioral invariant colocated with the tool (US-026), collected by the
/// Registry and injected into the system prompt.
const EDIT_GUIDELINES: &[&str] = &[
    "edit: old_string is searched in the CURRENT file contents on disk, not after \
     your other edits in the same turn. Re-anchor each edit on the current state \
     and include enough context for a unique anchor.",
];

/// Locating pass level reached (observability, US-025 AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchLevel {
    Exact,
    TrimEnd,
    Trim,
    Unicode,
}

impl MatchLevel {
    fn label(self) -> &'static str {
        match self {
            MatchLevel::Exact => "level 1: exact",
            MatchLevel::TrimEnd => "level 2: trim_end",
            MatchLevel::Trim => "level 3: trim",
            MatchLevel::Unicode => "level 4: Unicode normalization",
        }
    }
}

/// Successful location of the anchor.
enum Anchor {
    /// Exact substring match (preserves intra-line editing, exact offsets).
    Substring,
    /// Line window `[start, start+len)` found by a fuzzy pass.
    Lines {
        start: usize,
        len: usize,
        level: MatchLevel,
    },
}

enum LocateError {
    /// \>= 2 matches on a pass (or on exact) -> irreducible ambiguity
    /// (more permissive passes would only merge further, never
    /// distinguish). No level: it only documents SUCCESS (AC4).
    Ambiguous { count: usize },
    /// No match after the 4 passes.
    NotFound,
}

/// Locates `old` in `content` through 4 passes (exact -> trim_end -> trim -> Unicode).
/// The exact pass stays a SUBSTRING one (intra-line editing preserved); the fuzzy
/// passes are LINE BY LINE, to apply the replacement on the ORIGINAL lines.
/// Deterministic resolution: the first pass with a UNIQUE match wins;
/// otherwise >= 2 -> ambiguous, 0 everywhere -> not found.
fn locate(content: &str, old: &str) -> Result<Anchor, LocateError> {
    // Pass 1: exact substring.
    let exact = content.matches(old).count();
    if exact == 1 {
        return Ok(Anchor::Substring);
    }
    // Passes 2-4: line by line, increasing normalization.
    let content_lines: Vec<&str> = content.split('\n').collect();
    let old_lines: Vec<&str> = old.split('\n').collect();
    for level in [MatchLevel::TrimEnd, MatchLevel::Trim, MatchLevel::Unicode] {
        let windows = find_windows(&content_lines, &old_lines, level);
        match windows.len() {
            0 => continue,
            1 => {
                return Ok(Anchor::Lines {
                    start: windows[0],
                    len: old_lines.len(),
                    level,
                });
            }
            n => return Err(LocateError::Ambiguous { count: n }),
        }
    }
    if exact >= 2 {
        Err(LocateError::Ambiguous { count: exact })
    } else {
        Err(LocateError::NotFound)
    }
}

/// Start indices of the `content_lines` windows matching `old_lines` under
/// the normalization of `level`.
fn find_windows(content_lines: &[&str], old_lines: &[&str], level: MatchLevel) -> Vec<usize> {
    if old_lines.is_empty() || old_lines.len() > content_lines.len() {
        return Vec::new();
    }
    // Normalization precomputed ONCE per line (otherwise each file line was
    // renormalized up to `old_lines.len()` times -> O(n*m) allocations).
    let content_n: Vec<String> = content_lines.iter().map(|l| norm(l, level)).collect();
    let pat: Vec<String> = old_lines.iter().map(|l| norm(l, level)).collect();
    let last = content_lines.len() - old_lines.len();
    let mut out = Vec::new();
    for i in 0..=last {
        if content_n[i..i + pat.len()] == pat[..] {
            out.push(i);
        }
    }
    out
}

/// Normalizes a line according to the pass level (increasing permissiveness).
fn norm(line: &str, level: MatchLevel) -> String {
    match level {
        MatchLevel::Exact | MatchLevel::TrimEnd => line.trim_end().to_string(),
        MatchLevel::Trim => line.trim().to_string(),
        MatchLevel::Unicode => normalize_unicode_line(line),
    }
}

/// Unicode normalization table (US-025 AC5), taken from Pi/Codex CLI: dashes
/// U+2010-U+2015 & U+2212 -> `-`, typographic quotes -> ASCII, NBSP & special
/// spaces -> ` `. `trim` included (most permissive pass). No NFKC (explicit
/// character table, without an external dependency).
fn normalize_unicode_line(line: &str) -> String {
    let mapped: String = line
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    mapped.trim().to_string()
}

/// Replaces the window of ORIGINAL lines `[start, start+len)` with `new`,
/// joining the rest byte for byte (content outside the target intact). Preserves the
/// dominant terminator: on a CRLF file, the segments outside the target keep their
/// `\r`; we align the `new` lines on it (otherwise the fuzzy pass, which matches by
/// stripping `\r`, would reinject `new` in LF -> MIXED line endings in the edited
/// region).
fn apply_line_window(content: &str, start: usize, len: usize, new: &str) -> String {
    let segs: Vec<&str> = content.split('\n').collect();
    let crlf = content.contains("\r\n");
    let mut result: Vec<String> = Vec::with_capacity(segs.len());
    result.extend(segs[..start].iter().map(|s| (*s).to_string()));
    for nl in new.split('\n') {
        // normalizes the line endings of `new` onto those of the file (strip then
        // reattach `\r` when CRLF) -> coherent edited region.
        let core = nl.strip_suffix('\r').unwrap_or(nl);
        result.push(if crlf {
            format!("{core}\r")
        } else {
            core.to_string()
        });
    }
    result.extend(segs[start + len..].iter().map(|s| (*s).to_string()));
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_substring_is_level_1_and_intramline() {
        // intra-line "UNIQUE" -> exact substring match (offsets preserved).
        assert!(matches!(
            locate("alpha UNIQUE beta\n", "UNIQUE"),
            Ok(Anchor::Substring)
        ));
    }

    #[test]
    fn trim_end_pass_resolves_trailing_whitespace() {
        // MULTI-line anchor where one file line carries a trailing space absent
        // from the anchor: the exact substring fails, line-by-line trim_end solves it.
        let content = "foo \nbar\nbaz\n";
        assert!(matches!(
            locate(content, "foo\nbar"),
            Ok(Anchor::Lines {
                start: 0,
                len: 2,
                level: MatchLevel::TrimEnd
            })
        ));
    }

    #[test]
    fn trim_pass_resolves_leading_whitespace() {
        // Whitespace difference at the START of the line: trim_end is not enough, trim is.
        let content = "  foo \nbar\nbaz\n";
        assert!(matches!(
            locate(content, "foo\nbar"),
            Ok(Anchor::Lines {
                start: 0,
                len: 2,
                level: MatchLevel::Trim
            })
        ));
    }

    #[test]
    fn multi_line_exact_substring_is_level_1() {
        // Multi-line anchor PRESENT as is -> exact pass (substring).
        assert!(matches!(
            locate("x\nfoo\nbar\ny\n", "foo\nbar"),
            Ok(Anchor::Substring)
        ));
    }

    #[test]
    fn unicode_pass_absorbs_nbsp_dash_and_quotes() {
        // file: NBSP + em-dash + smart quotes; anchor: ASCII.
        let content = "let s = \u{201C}h\u{00A0}\u{2014}llo\u{201D};\nx\n";
        let r = locate(content, "let s = \"h -llo\";");
        assert!(
            matches!(
                r,
                Ok(Anchor::Lines {
                    level: MatchLevel::Unicode,
                    start: 0,
                    len: 1
                })
            ),
            "Unicode pass should absorb NBSP, dash, and quotes"
        );
    }

    #[test]
    fn ambiguous_two_lines_rejected_without_mutation() {
        let r = locate("dup\ndup\n", "dup");
        assert!(matches!(r, Err(LocateError::Ambiguous { .. })));
    }

    #[test]
    fn not_found_after_all_passes() {
        assert!(matches!(
            locate("alpha\nbeta\n", "gamma"),
            Err(LocateError::NotFound)
        ));
    }

    #[test]
    fn apply_window_keeps_surrounding_bytes_intact() {
        // multi-line anchor located by trim_end (trailing space breaking the substring).
        let content = "a\nfoo \nbar\nz\n";
        match locate(content, "foo\nbar") {
            Ok(Anchor::Lines { start, len, .. }) => {
                assert_eq!((start, len), (1, 2));
                let out = apply_line_window(content, start, len, "NEW");
                assert_eq!(out, "a\nNEW\nz\n", "untargeted lines intact");
            }
            _ => unreachable!("expected window localization"),
        }
    }

    #[test]
    fn apply_window_preserves_crlf_endings() {
        // CRLF file: the fuzzy pass matches (trim_end strips \r) but the
        // replacement must stay CRLF, not introduce mixed line endings.
        let content = "a\r\nfoo \r\nbar\r\nz\r\n";
        match locate(content, "foo\nbar") {
            Ok(Anchor::Lines { start, len, .. }) => {
                let out = apply_line_window(content, start, len, "NEW1\nNEW2");
                assert_eq!(
                    out, "a\r\nNEW1\r\nNEW2\r\nz\r\n",
                    "CRLF preserved throughout"
                );
            }
            _ => unreachable!("expected window localization"),
        }
    }

    #[test]
    fn unicode_table_covers_required_chars() {
        assert_eq!(normalize_unicode_line("\u{2010}\u{2015}\u{2212}"), "---");
        assert_eq!(normalize_unicode_line("\u{2018}\u{2019}"), "''");
        assert_eq!(normalize_unicode_line("\u{201C}\u{201D}"), "\"\"");
        assert_eq!(normalize_unicode_line("a\u{00A0}b"), "a b");
    }
}
