//! Live progress indicators (US-044/045): animated shimmer and elapsed time.
//! Signals that the session is working (not frozen).
//!
//! Pure and CLOCK-FREE: the clock lives in the `agent-cli` loop, which advances
//! `AppState::spinner_tick` (~10 fps) and provides the duration. This module only
//! picks the shimmer style for a given tick and formats the duration, so that
//! `render` stays testable through `TestBackend`.
//!
//! Reduced-motion degradation (`NO_COLOR` / `PYXIS_REDUCED_MOTION`): the animated
//! shimmer becomes static text.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::theme::Theme;

const SHIMMER_PADDING: usize = 10;
const SHIMMER_SWEEP_TICKS: usize = 20;
const SHIMMER_BAND_HALF_WIDTH: f32 = 5.0;

/// Compact duration: `Ns` below a minute, then `Nm Ns`.
pub(crate) fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m {:02}s", s / 3600, (s % 3600) / 60, s % 60)
    }
}

pub(crate) fn shimmer_text(
    text: &str,
    tick: usize,
    reduced_motion: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if reduced_motion {
        return vec![Span::styled(text.to_string(), theme.dim())];
    }

    let chars: Vec<char> = text.chars().collect();
    let period = chars.len() + SHIMMER_PADDING * 2;
    let sweep = SHIMMER_SWEEP_TICKS.max(1);
    let pos = ((tick % sweep) as f32 / sweep as f32 * period as f32) as isize;

    chars
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let i_pos = i as isize + SHIMMER_PADDING as isize;
            let dist = (i_pos - pos).abs() as f32;
            let intensity = if dist <= SHIMMER_BAND_HALF_WIDTH {
                let x = std::f32::consts::PI * (dist / SHIMMER_BAND_HALF_WIDTH);
                0.5 * (1.0 + x.cos())
            } else {
                0.0
            };
            Span::styled(ch.to_string(), shimmer_style(intensity, theme))
        })
        .collect()
}

fn shimmer_style(intensity: f32, theme: &Theme) -> Style {
    if theme.truecolor() {
        let t = intensity.clamp(0.0, 1.0);
        let base = 0x8a as f32;
        let highlight = 0xe4 as f32;
        let value = (base + (highlight - base) * t * 0.9).round() as u8;
        let mut style = Style::default().fg(Color::Rgb(value, value, value));
        if t >= 0.6 {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    } else if intensity < 0.2 {
        Style::default().add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_format_is_compact() {
        assert_eq!(fmt_duration(Duration::from_secs(4)), "4s");
        assert_eq!(fmt_duration(Duration::from_secs(75)), "1m 15s");
        assert_eq!(fmt_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(fmt_duration(Duration::from_secs(3661)), "1h 01m 01s");
    }

    #[test]
    fn shimmer_rebuilds_text_and_reduced_motion_is_plain() {
        let theme = Theme::new(true);
        let spans = shimmer_text("Working", 8, false, &theme);
        let rebuilt: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(rebuilt, "Working");
        assert!(spans.iter().any(|span| span.style != theme.dim()));

        let reduced = shimmer_text("Working", 8, true, &theme);
        assert_eq!(reduced.len(), 1);
        assert_eq!(reduced[0].content.as_ref(), "Working");
    }
}
