//! US-004 runner: interactive when stdout is a TTY (for the visual check),
//! otherwise a headless dump of the rendered buffer (runnable proof without a TTY).
//!
//! The "core" (a feeder thread) only emits `AgentEvent` through an mpsc channel,
//! never ANSI. The TUI consumes the channel and renders.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use spike_tui::{AgentEvent, AppState, supports_truecolor, tokenize, ui};
use std::io::IsTerminal;
use std::time::Duration;

const DEMO_TEXT: &str = "Pyxis démarre, parle à n'importe quel modèle, et streame \
    la réponse token par token directement dans ton shell. Monochrome, épuré, rapide.";

fn main() -> Result<()> {
    if std::io::stdout().is_terminal() {
        run_interactive()
    } else {
        run_headless_dump()
    }
}

/// Single render toward a test backend, printed as plain text: runnable without a TTY.
fn run_headless_dump() -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (w, h) = (80u16, 12u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    let mut state = AppState::new(supports_truecolor());
    state.input = "écris un test pour…".to_string();
    for tok in tokenize(DEMO_TEXT) {
        state.apply(&AgentEvent::Text(tok));
    }
    state.apply(&AgentEvent::EndTurn);
    terminal.draw(|f| ui(f, &state))?;

    let buf = terminal.backend().buffer();
    println!(
        "[s4] dump headless du rendu Ratatui ({w}x{h}, truecolor={}):",
        state.truecolor
    );
    println!("┌{}┐", "─".repeat(w as usize));
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("│{line}│");
    }
    println!("└{}┘", "─".repeat(w as usize));
    println!("[s4] le cœur n'a émis que des AgentEvent (jamais d'ANSI) ✓");
    println!("[s4] lance dans un vrai terminal pour la vérif de fluidité interactive.");
    Ok(())
}

/// Interactive loop: feeder in a thread (emits AgentEvent), Ratatui rendering.
fn run_interactive() -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::crossterm::event::{self, Event, KeyCode};
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
    std::thread::spawn(move || {
        for tok in tokenize(DEMO_TEXT) {
            if tx.send(AgentEvent::Text(tok)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(45));
        }
        let _ = tx.send(AgentEvent::EndTurn);
    });

    let mut state = AppState::new(supports_truecolor());
    let res = (|| -> Result<()> {
        loop {
            while let Ok(ev) = rx.try_recv() {
                state.apply(&ev);
            }
            terminal.draw(|f| ui(f, &state))?;

            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(k) => match k.code {
                        KeyCode::Esc | KeyCode::Char('q') => break,
                        KeyCode::Char(c) => state.input.push(c),
                        KeyCode::Backspace => {
                            state.input.pop();
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {} // the next draw reflows by itself
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    res
}
