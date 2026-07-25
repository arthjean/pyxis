//! Rendering palette (US-032). Aesthetics: **monochrome + one orbital sky-blue accent**. The
//! hierarchy comes from weight and tint, not from color. Color is
//! RESERVED for the functional: diff tones (add/remove) and `success`. Without
//! truecolor (AC4), everything degrades to 16 colors / modifiers without
//! losing the distinction (the layout is unchanged).
//!
//! Extracted from `render.rs` to centralize the colors and keep the rendering pure.

use ratatui::style::{Color, Modifier, Style};

/// Palette: graphite + one sky-blue accent + functional tones (error, diff,
/// success). `truecolor` drives the degradation.
pub struct Theme {
    truecolor: bool,
}

impl Theme {
    pub fn new(truecolor: bool) -> Self {
        Self { truecolor }
    }

    /// Does the terminal support 24-bit color? (consumed by the logo rendering, which
    /// interpolates a continuous tint only in truecolor.)
    pub fn truecolor(&self) -> bool {
        self.truecolor
    }

    // ── Monochrome chrome + accent ──────────────────────────────────────────────

    pub fn fg(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xf2, 0xf0, 0xea))
        } else {
            Style::default()
        }
    }
    pub fn dim(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x8e, 0x94, 0x9e))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn faint(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x50, 0x57, 0x62))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn accent(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x6c, 0xcb, 0xff))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }
    pub fn error(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xff, 0x6b, 0x78))
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    }
    /// Background of the selected line (command menu): dark blue veil in
    /// truecolor, reverse video in 16 colors.
    pub fn selection(&self) -> Style {
        if self.truecolor {
            Style::default().bg(Color::Rgb(0x0f, 0x23, 0x34))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
    /// Horizontal rule of the composer, visible without enclosing the input in a block.
    pub fn composer_rule(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x2a, 0x2f, 0x37))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    /// Highlight of a `/skill` inserted in the input: sky-blue chip on a dark background.
    pub fn skill_chip(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0x6c, 0xcb, 0xff))
                .bg(Color::Rgb(0x0f, 0x23, 0x34))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    // ── FUNCTIONAL tones (color allowed because it carries meaning) ──────────────

    /// Success / confirmation (e.g. goal reached).
    pub fn success(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x78, 0xc9, 0x8a))
        } else {
            Style::default().fg(Color::Green)
        }
    }
    /// Added line of a diff: dark green background + light text (truecolor); in 16
    /// colors, plain green (the `+` sign also carries the meaning, not only the color).
    pub fn diff_add(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xbd, 0xec, 0xc9))
                .bg(Color::Rgb(0x11, 0x28, 0x16))
        } else {
            Style::default().fg(Color::Green)
        }
    }
    /// Removed line of a diff: dark red background + light text (truecolor).
    pub fn diff_remove(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xff, 0xc7, 0xcf))
                .bg(Color::Rgb(0x30, 0x13, 0x17))
        } else {
            Style::default().fg(Color::Red)
        }
    }
    /// WORD-BY-WORD added segment (intra-line emphasis): saturated green background.
    pub fn diff_add_word(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xf5, 0xff, 0xf7))
                .bg(Color::Rgb(0x2a, 0x6a, 0x39))
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::REVERSED)
        }
    }
    /// WORD-BY-WORD removed segment: saturated red background.
    pub fn diff_remove_word(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xff, 0xf0, 0xf2))
                .bg(Color::Rgb(0x7b, 0x2a, 0x35))
        } else {
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::REVERSED)
        }
    }
}
