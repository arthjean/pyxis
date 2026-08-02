//! Syntax coloring of code blocks and diffs (US-040 spike + US-042).
//!
//! ## Engine choice (US-040 spike decision)
//! `syntect` 5.3 in **`default-fancy`** (`fancy-regex` regexes, **pure Rust**: no C
//! toolchain, unlike the oniguruma default) + **`two-face`** 0.5 for the
//! grammars. Rationale:
//! - **No C dependency** -> binary distributable without a toolchain (hard constraint
//!   of the PRD); `synoptic` is also C-free but its regex rules are coarser
//!   than the Sublime grammars.
//! - **Quality**: faithful Sublime grammars. The DEFAULT syntect set does NOT
//!   cover TypeScript nor TOML (verified on the packdump); `two-face` embeds
//!   the curated `bat` set covering Rust/TS-JS/JSON/TOML/Markdown (the 5 required
//!   languages). `two-face` is only embedded dumps (pure Rust, syntect 5.3).
//! - **Battle-tested**: exactly the Codex CLI stack (Rust + ratatui).
//! - **Cost**: ~3 MB of binary, acceptable. Ruled out: `synoptic` (poorer
//!   grammars), `syntect`+`onig` (C toolchain).
//!
//! ## Performance (see US-041)
//! Syntect coloring is STATEFUL line by line and expensive -> it NEVER runs
//! per frame: `render.rs` memoizes the already colored lines (per-block cache).
//! The `SyntaxSet` and the `Theme` are loaded ONLY once (global `OnceLock`).
//!
//! ## Degradation
//! Coloring applies ONLY in truecolor: syntect embeds no 16-color ANSI theme
//! and hardcoding RGB outside truecolor would corrupt the palette
//! (Codex lesson). Without truecolor -> `None` -> the caller falls back on the
//! existing monochrome (dim) rendering. Language not covered -> `None` (neutral text).

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use crate::theme::Theme as UiTheme;

/// Length bound (bytes) past which a line is NOT colored:
/// avoids running the regex engine on a giant minified line (linear cost per
/// grammar rule). Generous: real code lines are well below it.
const MAX_HL_BYTES: usize = 16 * 1024;
const MAX_CODE_BLOCK_BYTES: usize = 128 * 1024;
const MAX_CODE_BLOCK_LINES: usize = 2_000;

/// Grammars (two-face: Rust/TS-JS/JSON/TOML/Markdown, ...), loaded once.
fn syntaxes() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(two_face::syntax::extra_newlines)
}

/// Codex's default syntax theme on dark terminals.
fn theme() -> &'static Theme {
    static TH: OnceLock<Theme> = OnceLock::new();
    TH.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

/// Resolves a language label (or an extension) into a grammar. Normalizes common
/// aliases to the canonical extension (reliable through `find_syntax_by_extension`),
/// falling back on the token. `None` when not covered -> no coloring.
fn syntax_for(ss: &'static SyntaxSet, lang: &str) -> Option<&'static SyntaxReference> {
    let l = lang.trim().to_ascii_lowercase();
    if l.is_empty() {
        return None;
    }
    let ext = match l.as_str() {
        "rust" => "rs",
        "typescript" => "ts",
        "javascript" | "mjs" | "cjs" => "js",
        "markdown" => "md",
        "shell" | "bash" | "zsh" | "sh" => "sh",
        "yml" => "yaml",
        other => other,
    };
    ss.find_syntax_by_extension(ext)
        .or_else(|| ss.find_syntax_by_token(&l))
}

/// Ratatui color from a syntect color (RGB; alpha and background ignored: we
/// keep our own background, and take ONLY the foreground tint).
fn to_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn to_style(style: syntect::highlighting::Style) -> Style {
    let mut out = Style::default().fg(to_color(style.foreground));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    out
}

/// Colors a MULTI-LINE code block: stateful line-by-line rendering (the context of
/// multi-line strings/comments is preserved). Returns the colored spans per
/// line (without indentation: the caller lays out the gutter). `None` when not truecolor
/// or the language is not covered -> the caller falls back on the neutral (dim) rendering.
pub fn code_block(code: &str, lang: &str, ui: &UiTheme) -> Option<Vec<Vec<Span<'static>>>> {
    if !ui.truecolor() {
        return None;
    }
    if code.len() > MAX_CODE_BLOCK_BYTES
        || code.lines().take(MAX_CODE_BLOCK_LINES + 1).count() > MAX_CODE_BLOCK_LINES
        || code.lines().any(|line| line.len() > MAX_HL_BYTES)
    {
        return None;
    }
    let ss = syntaxes();
    let syntax = syntax_for(ss, lang)?;
    let theme = theme();
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        // A coloring error (rare) must not break the rendering: we stop and
        // the caller falls back on neutral for the rest.
        let ranges = h.highlight_line(line, ss).ok()?;
        out.push(spans_from_ranges(&ranges));
    }
    Some(out)
}

/// Syntax color per CHARACTER for an isolated line (state reset:
/// best-effort on multi-line constructs, enough for a hunk line).
/// Used to tint the content of a diff without touching the add/remove background
/// (US-042). `None` when not truecolor or the language is not covered.
pub fn line_colors(line: &str, lang: &str, ui: &UiTheme) -> Option<Vec<Color>> {
    if !ui.truecolor() {
        return None;
    }
    // Cost bound: a giant line (minified file, diff of a `write`) would run
    // every regex rule of the grammar over its whole content. Past the threshold,
    // no coloring (the diff stays readable, +/- background kept). Only the first
    // `width` chars are displayed anyway.
    if line.len() > MAX_HL_BYTES {
        return None;
    }
    let ss = syntaxes();
    let syntax = syntax_for(ss, lang)?;
    let theme = theme();
    let mut h = HighlightLines::new(syntax, theme);
    let with_nl = format!("{line}\n");
    let ranges = h.highlight_line(&with_nl, ss).ok()?;
    let mut cols = Vec::new();
    for (st, text) in ranges {
        let color = to_color(st.foreground);
        for _ in text.trim_end_matches(['\n', '\r']).chars() {
            cols.push(color);
        }
    }
    Some(cols)
}

/// Converts the syntect ranges of a line into ratatui spans (fg tint only, default
/// background). The line terminator is dropped; empty segments are discarded.
fn spans_from_ranges(ranges: &[(syntect::highlighting::Style, &str)]) -> Vec<Span<'static>> {
    ranges
        .iter()
        .filter_map(|(st, text)| {
            let t = text.trim_end_matches(['\n', '\r']);
            (!t.is_empty()).then(|| Span::styled(t.to_string(), to_style(*st)))
        })
        .collect()
}

/// Guesses the language of a file from its path extension (to color a
/// diff). `None` when there is no usable extension.
pub fn lang_from_path(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme as UiTheme;

    #[test]
    fn no_color_without_truecolor() {
        let ui = UiTheme::new(false);
        assert!(code_block("let x = 1;", "rust", &ui).is_none());
        assert!(line_colors("let x = 1;", "rust", &ui).is_none());
    }

    #[test]
    fn required_languages_resolve() {
        // The 5 languages required by US-042 must have a grammar (two-face).
        let ss = syntaxes();
        for lang in ["rust", "ts", "js", "json", "toml", "md"] {
            assert!(
                syntax_for(ss, lang).is_some(),
                "grammaire manquante pour {lang}"
            );
        }
    }

    #[test]
    fn unknown_language_falls_back_to_none() {
        let ui = UiTheme::new(true);
        assert!(
            code_block("???", "langage-bidon-inconnu", &ui).is_none(),
            "langage inconnu → pas de coloration (texte neutre)"
        );
    }

    #[test]
    fn code_block_colors_in_truecolor() {
        let ui = UiTheme::new(true);
        let lines = code_block("let x = 1;\nlet y = 2;\n", "rust", &ui)
            .expect("rust devrait être coloré en truecolor");
        assert_eq!(lines.len(), 2);
        // At least one colored span (RGB) on the first line.
        assert!(
            lines[0]
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        );
    }

    #[test]
    fn syntax_theme_matches_codex_dark_default() {
        assert_eq!(
            theme(),
            two_face::theme::extra().get(EmbeddedThemeName::CatppuccinMocha)
        );
    }

    #[test]
    fn line_colors_match_char_count() {
        let ui = UiTheme::new(true);
        let line = "let x = 1;";
        let cols = line_colors(line, "rust", &ui).expect("expected highlighting");
        assert_eq!(
            cols.len(),
            line.chars().count(),
            "one color per character for diff alignment"
        );
    }

    #[test]
    fn line_colors_handles_multibyte() {
        // Critical contract for the diff overlay: ONE color per `char`, even on
        // multi-byte characters (otherwise the tint drifts silently).
        let ui = UiTheme::new(true);
        let line = "let tea = 1; // ☕";
        let cols = line_colors(line, "rust", &ui).expect("expected highlighting");
        assert_eq!(
            cols.len(),
            line.chars().count(),
            "one color per char, including multibyte"
        );
    }

    #[test]
    fn line_colors_skips_giant_line() {
        // Cost bound: a line past the threshold is not colored (no regex
        // on a giant minified content) -> neutral fallback on the caller side.
        let ui = UiTheme::new(true);
        let giant = "a".repeat(MAX_HL_BYTES + 1);
        assert!(
            line_colors(&giant, "rust", &ui).is_none(),
            "giant line should not be highlighted"
        );
    }

    #[test]
    fn code_block_skips_giant_input() {
        let ui = UiTheme::new(true);
        let giant_block = "a\n".repeat(MAX_CODE_BLOCK_LINES + 1);
        assert!(code_block(&giant_block, "rust", &ui).is_none());

        let giant_line = "a".repeat(MAX_HL_BYTES + 1);
        assert!(code_block(&giant_line, "rust", &ui).is_none());
    }

    #[test]
    fn lang_from_path_reads_extension() {
        assert_eq!(lang_from_path("src/main.rs").as_deref(), Some("rs"));
        assert_eq!(lang_from_path("a/b/c.toml").as_deref(), Some("toml"));
        assert_eq!(lang_from_path("Makefile"), None);
        assert_eq!(lang_from_path(".gitignore"), None);
    }
}
