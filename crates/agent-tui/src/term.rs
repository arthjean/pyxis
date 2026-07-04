//! Setup/teardown du terminal : raw mode + écran alternatif (crossterm). Isolé
//! ici pour que le rendu (`render.rs`) reste pur et testable sans terminal réel.

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

/// Entre en mode terminal interactif. Le chemin historique utilise l'alt-screen
/// avec capture souris ; le chemin parity garde le scrollback terminal natif.
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
    let inline_height = size().map(|(_, rows)| rows.max(1)).unwrap_or(24);
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

/// Restaure le terminal (à appeler en sortie, y compris sur erreur).
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

/// Détection truecolor → choix de la dégradation monochrome (US-019 AC4).
pub fn supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}
