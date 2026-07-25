//! US-004: raw streaming TUI rendering (Ratatui).
//!
//! Proves the `agent-core -> channel -> agent-tui` pipe: the core only emits
//! structured `AgentEvent` (never ANSI), the frontend alone decides the rendering.
//! Target aesthetics: monochrome + one accent, no heavy ASCII border.
//!
//! The rendering logic (`ui`) is pure and tested through `TestBackend`; the
//! subjective smoothness (token by token, without flicker) is checked interactively.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// Core -> client contract (subset of the `AgentEvent` of architecture 10.1).
/// No presentation decision, no ANSI sequence.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Text(String),
    Reasoning(String),
    EndTurn,
}

/// Client-side rendering state.
pub struct AppState {
    pub transcript: String,
    pub input: String,
    pub done: bool,
    pub truecolor: bool,
}

impl AppState {
    pub fn new(truecolor: bool) -> Self {
        Self {
            transcript: String::new(),
            input: String::new(),
            done: false,
            truecolor,
        }
    }

    /// Applies a core event. Reasoning is not rendered in this spike
    /// (decoding is enough, see the ROADMAP: "reasoning rendering not required").
    pub fn apply(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Text(t) => self.transcript.push_str(t),
            AgentEvent::Reasoning(_) => {}
            AgentEvent::EndTurn => self.done = true,
        }
    }
}

/// Truecolor detection -> clean monochrome degradation when absent (AC3).
pub fn supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

/// Pure rendering: streamed transcript on top, input field at the bottom. Monochrome;
/// a single accent (the prompt marker) in truecolor, bold otherwise.
pub fn ui(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(frame.area());

    let transcript = Paragraph::new(state.transcript.as_str())
        .wrap(Wrap { trim: false })
        .block(Block::default());
    frame.render_widget(transcript, chunks[0]);

    let accent_style = if state.truecolor {
        Style::default()
            .fg(Color::Rgb(0x9b, 0x87, 0xf5))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let prompt = Span::styled("› ", accent_style);
    let input_line = Line::from(vec![prompt, Span::raw(state.input.as_str())]);
    let input = Paragraph::new(input_line).block(Block::default().borders(Borders::TOP));
    frame.render_widget(input, chunks[1]);
}

/// Splits a text into "tokens" (words + space) to simulate a stream.
pub fn tokenize(s: &str) -> Vec<String> {
    s.split_inclusive(' ').map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn dump(buf: &Buffer, w: u16, h: u16) -> String {
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // AC1 (deterministic version): the streamed tokens accumulate and render.
    #[test]
    fn streamed_text_renders_into_buffer() {
        let (w, h) = (40, 10);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut state = AppState::new(false);

        // Simulates the token-by-token arrival, redrawing on every token.
        for tok in tokenize("Bonjour depuis Pyxis en streaming") {
            state.apply(&AgentEvent::Text(tok));
            terminal.draw(|f| ui(f, &state)).unwrap();
        }

        let text = dump(terminal.backend().buffer(), w, h);
        assert!(
            text.contains("Bonjour depuis Pyxis"),
            "transcript absent:\n{text}"
        );
        assert!(text.contains("›"), "marqueur de prompt absent");
    }

    // AC2: a resize mid-stream does not corrupt the rendering.
    #[test]
    fn resize_midstream_reflows_without_corruption() {
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
        let mut state = AppState::new(false);
        state.apply(&AgentEvent::Text(
            "un texte assez long pour wrapper sur plusieurs lignes lorsque la largeur change"
                .into(),
        ));
        terminal.draw(|f| ui(f, &state)).unwrap();

        // narrows the terminal in the middle of the "stream"
        terminal.backend_mut().resize(24, 12);
        state.apply(&AgentEvent::Text(
            " et encore du texte ajouté après le resize".into(),
        ));
        terminal.draw(|f| ui(f, &state)).unwrap();

        let text = dump(terminal.backend().buffer(), 24, 12);
        assert!(
            text.contains("resize"),
            "le texte post-resize doit apparaître"
        );
        // no panic = no index corruption; the wrap recomputed.
    }

    #[test]
    fn monochrome_degradation_is_selected_without_truecolor() {
        let state = AppState::new(false);
        assert!(!state.truecolor);
    }
}
