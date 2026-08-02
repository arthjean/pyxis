//! Rendering palette aligned with the Codex TUI style guide: terminal-default
//! primary text, dim secondary text, ANSI cyan for interactive accents, green
//! for success/additions, red for failures/deletions, and magenta for Codex.
//! Custom RGB values are limited to low-contrast surfaces and diff backgrounds.
//!
//! Extracted from `render.rs` to centralize the colors and keep the rendering pure.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::{Color, Modifier, Style};

/// Colour depth of the terminal this process is attached to.
///
/// A history cell renders from `display_lines(width)` alone, with no route back
/// to application state, so the one property it needs about the terminal lives
/// where the terminal itself does: in the process. Set once at startup by the
/// app loop; the default suits the snapshot tests, which assert on full colour.
static TRUECOLOR: AtomicBool = AtomicBool::new(true);

/// Records what the terminal supports. Call once, before the first render.
pub fn set_truecolor(enabled: bool) {
    TRUECOLOR.store(enabled, Ordering::Relaxed);
}

pub fn truecolor_enabled() -> bool {
    TRUECOLOR.load(Ordering::Relaxed)
}

/// Semantic Codex palette. `truecolor` is only needed for subtle backgrounds
/// and continuous logo rendering; semantic foregrounds stay on ANSI colors so
/// the terminal theme controls their exact appearance.
pub struct Theme {
    truecolor: bool,
}

impl Theme {
    pub fn new(truecolor: bool) -> Self {
        Self { truecolor }
    }

    /// Palette matching the terminal this process is attached to.
    pub fn current() -> Self {
        Self::new(truecolor_enabled())
    }

    /// Does the terminal support 24-bit color? (consumed by the logo rendering, which
    /// interpolates a continuous tint only in truecolor.)
    pub fn truecolor(&self) -> bool {
        self.truecolor
    }

    // ── Primary, secondary, and interactive chrome ─────────────────────────────

    pub fn fg(&self) -> Style {
        Style::default()
    }

    pub fn dim(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    pub fn faint(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    pub fn accent(&self) -> Style {
        Style::default().fg(Color::Cyan)
    }

    pub fn brand(&self) -> Style {
        Style::default().fg(Color::Magenta)
    }

    pub fn error(&self) -> Style {
        Style::default().fg(Color::Red)
    }

    /// Low-contrast surface used by user messages, the composer, and menus.
    /// Codex computes this by blending white over the detected dark terminal
    /// background. Pyxis has no terminal-palette probe, so its truecolor path
    /// uses the neutral result for a black background and otherwise falls back
    /// to the terminal background.
    pub fn user_message(&self) -> Style {
        if self.truecolor {
            Style::default().bg(Color::Rgb(0x1e, 0x1e, 0x1e))
        } else {
            Style::default()
        }
    }

    /// Selected controls use the same surface as Codex menus; their marker and
    /// active text carry the cyan accent.
    pub fn selection(&self) -> Style {
        self.user_message()
    }

    /// Horizontal rule of the composer.
    pub fn composer_rule(&self) -> Style {
        self.dim()
    }

    /// Invitation shown in an empty composer.
    pub fn composer_placeholder(&self) -> Style {
        self.dim()
    }

    /// Highlight of a `/skill`, `@file`, or slash command in the input.
    pub fn skill_chip(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    // ── FUNCTIONAL tones (color allowed because it carries meaning) ──────────────

    /// Success / confirmation (e.g. goal reached).
    pub fn success(&self) -> Style {
        Style::default().fg(Color::Green)
    }

    /// Added line of a diff. The truecolor background is Codex's dark palette.
    pub fn diff_add(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Green)
                .bg(Color::Rgb(0x21, 0x3a, 0x2b))
        } else {
            Style::default().fg(Color::Green)
        }
    }

    /// Removed line of a diff. The truecolor background is Codex's dark palette.
    pub fn diff_remove(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Red)
                .bg(Color::Rgb(0x4a, 0x22, 0x1d))
        } else {
            Style::default().fg(Color::Red)
        }
    }
    /// Word-level addition emphasis over the same Codex line background.
    pub fn diff_add_word(&self) -> Style {
        self.diff_add().add_modifier(Modifier::BOLD)
    }

    /// Word-level removal emphasis over the same Codex line background.
    pub fn diff_remove_word(&self) -> Style {
        self.diff_remove().add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_foregrounds_follow_the_codex_ansi_palette() {
        let theme = Theme::new(true);

        assert_eq!(theme.fg(), Style::default());
        assert_eq!(theme.dim(), Style::default().add_modifier(Modifier::DIM));
        assert_eq!(theme.faint(), theme.dim());
        assert_eq!(theme.accent().fg, Some(Color::Cyan));
        assert_eq!(theme.brand().fg, Some(Color::Magenta));
        assert_eq!(theme.success().fg, Some(Color::Green));
        assert_eq!(theme.error().fg, Some(Color::Red));
    }

    #[test]
    fn interactive_tokens_use_cyan_without_a_custom_background() {
        let style = Theme::new(true).skill_chip();

        assert_eq!(style.fg, Some(Color::Cyan));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dark_diff_backgrounds_match_codex() {
        let theme = Theme::new(true);

        assert_eq!(theme.diff_add().fg, Some(Color::Green));
        assert_eq!(theme.diff_add().bg, Some(Color::Rgb(0x21, 0x3a, 0x2b)));
        assert_eq!(theme.diff_remove().fg, Some(Color::Red));
        assert_eq!(theme.diff_remove().bg, Some(Color::Rgb(0x4a, 0x22, 0x1d)));
        assert_eq!(theme.diff_add_word().bg, theme.diff_add().bg);
        assert_eq!(theme.diff_remove_word().bg, theme.diff_remove().bg);
    }
}
