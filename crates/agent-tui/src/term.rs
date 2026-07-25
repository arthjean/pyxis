//! Terminal setup/teardown: raw mode + alternate screen (crossterm). Isolated
//! here so that rendering (`render.rs`) stays pure and testable without a real terminal.

use std::io::{self, Stdout};

use crossterm::cursor::MoveTo;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
#[cfg(not(feature = "codex_tui_parity"))]
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
#[cfg(feature = "codex_tui_parity")]
use crossterm::terminal::size;
use crossterm::terminal::{Clear, ClearType};
#[cfg(not(feature = "codex_tui_parity"))]
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
#[cfg(feature = "codex_tui_parity")]
use ratatui::{TerminalOptions, Viewport};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enters interactive terminal mode. The historical path uses the alt-screen
/// with mouse capture; the parity path keeps the native terminal scrollback.
pub fn enter() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    #[cfg(not(feature = "codex_tui_parity"))]
    if let Err(e) = execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    ) {
        let _ = disable_raw_mode();
        return Err(e);
    }
    #[cfg(feature = "codex_tui_parity")]
    let measured = size();
    #[cfg(feature = "codex_tui_parity")]
    crate::debug_log::log(&format!("enter: crossterm::size() = {measured:?}"));
    #[cfg(feature = "codex_tui_parity")]
    let inline_height = measured.map(|(_, rows)| rows.max(1)).unwrap_or(24);
    #[cfg(feature = "codex_tui_parity")]
    if let Err(e) = execute!(
        out,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0)
    ) {
        let _ = disable_raw_mode();
        return Err(e);
    }
    #[cfg(not(feature = "codex_tui_parity"))]
    let terminal = Terminal::new(CrosstermBackend::new(out));
    #[cfg(feature = "codex_tui_parity")]
    let terminal = Terminal::with_options(
        CrosstermBackend::new(out),
        TerminalOptions {
            viewport: Viewport::Inline(inline_height),
        },
    );
    match terminal {
        #[cfg(feature = "codex_tui_parity")]
        Ok(mut tui) => {
            crate::debug_log::log(&format!(
                "enter: inline_height={inline_height} viewport={:?}",
                tui.get_frame().area()
            ));
            Ok(tui)
        }
        #[cfg(not(feature = "codex_tui_parity"))]
        Ok(tui) => Ok(tui),
        Err(e) => {
            let mut out = io::stdout();
            #[cfg(not(feature = "codex_tui_parity"))]
            let _ = execute!(
                out,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            #[cfg(feature = "codex_tui_parity")]
            let _ = execute!(out, DisableBracketedPaste);
            let _ = disable_raw_mode();
            Err(e)
        }
    }
}

pub fn clear(tui: &mut Tui) -> io::Result<()> {
    tui.clear()?;
    execute!(tui.backend_mut(), Clear(ClearType::All), MoveTo(0, 0))?;
    Ok(())
}

/// True when the inline viewport no longer covers the whole terminal height.
///
/// `Viewport::Inline(h)` freezes `h` at construction time: ratatui then clamps to
/// `min(screen_height, h)`. A SHRUNK terminal is therefore tracked correctly, but
/// an ENLARGED terminal keeps the old height, and the whole rendering (transcript,
/// composer, status bar) stays locked inside that stale area.
#[cfg(feature = "codex_tui_parity")]
pub fn inline_viewport_stale(viewport_height: u16, screen_height: u16) -> bool {
    viewport_height < screen_height
}

/// Realigns the inline viewport on the current terminal height, starting from
/// a fresh `Terminal`: ratatui exposes no way to change the height of a
/// `Viewport::Inline`. Returns `true` when the rebuild happened.
///
/// The scrollback already emitted by `insert_before` is untouched: we clear the
/// visible screen and the next `draw` repaints from the state, as at startup.
///
/// The construction queries the cursor position; the caller MUST therefore guarantee
/// that no blocking `crossterm::event::read()` runs in parallel, otherwise the
/// terminal response stays captive of that reader and the request times out.
#[cfg(feature = "codex_tui_parity")]
pub fn sync_inline_viewport(tui: &mut Tui) -> io::Result<bool> {
    let screen = tui.size()?;
    let viewport = tui.get_frame().area();
    if !inline_viewport_stale(viewport.height, screen.height) {
        return Ok(false);
    }
    crate::debug_log::log(&format!(
        "sync: rebuild viewport={viewport:?} -> screen={screen:?}"
    ));
    let mut out = io::stdout();
    execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;
    *tui = Terminal::with_options(
        CrosstermBackend::new(out),
        TerminalOptions {
            viewport: Viewport::Inline(screen.height.max(1)),
        },
    )?;
    crate::debug_log::log(&format!(
        "sync: rebuilt viewport={:?}",
        tui.get_frame().area()
    ));
    Ok(true)
}

/// Restores the terminal (to be called on exit, including on error).
pub fn leave(tui: &mut Tui) -> io::Result<()> {
    let mut first_err: Option<io::Error> = None;
    #[cfg(not(feature = "codex_tui_parity"))]
    if let Err(e) = execute!(
        tui.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    ) {
        first_err = Some(e);
    }
    #[cfg(feature = "codex_tui_parity")]
    if let Err(e) = execute!(tui.backend_mut(), DisableBracketedPaste) {
        first_err = Some(e);
    }
    if let Err(e) = disable_raw_mode()
        && first_err.is_none()
    {
        first_err = Some(e);
    }
    if let Err(e) = tui.show_cursor()
        && first_err.is_none()
    {
        first_err = Some(e);
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Truecolor detection -> choice of the monochrome degradation (US-019 AC4).
pub fn supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

#[cfg(all(test, feature = "codex_tui_parity"))]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn viewport_is_stale_only_when_the_screen_grew() {
        assert!(inline_viewport_stale(24, 47), "terminal agrandi");
        assert!(!inline_viewport_stale(47, 47), "tailles alignées");
        assert!(
            !inline_viewport_stale(47, 24),
            "rétréci : ratatui clampe déjà"
        );
    }

    /// Pins down the ratatui constraint that forces rebuilding the terminal.
    /// Should this test break (height tracked automatically), `sync_inline_viewport`
    /// becomes useless and can disappear.
    #[test]
    fn ratatui_inline_viewport_keeps_its_initial_height_when_the_screen_grows() {
        let mut terminal = Terminal::with_options(
            TestBackend::new(20, 24),
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .expect("terminal inline");

        terminal.backend_mut().resize(20, 47);
        terminal.autoresize().expect("autoresize");

        let height = terminal.get_frame().area().height;
        assert_eq!(height, 24, "hauteur figée à la construction");
        assert!(inline_viewport_stale(height, 47));
    }
}
