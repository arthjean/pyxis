//! Setup/teardown du terminal : raw mode + écran alternatif (crossterm). Isolé
//! ici pour que le rendu (`render.rs`) reste pur et testable sans terminal réel.

use std::io::{self, Stdout};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Entre en mode plein écran (raw + alt screen + capture souris). La capture
/// souris route la molette vers l'app (scroll du transcript) ; contrepartie : la
/// sélection au clic-glissé passe par Shift (la copie native n'est plus directe).
pub fn enter() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(out))
}

/// Restaure le terminal (à appeler en sortie, y compris sur erreur).
pub fn leave(tui: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(tui.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    tui.show_cursor()?;
    Ok(())
}

/// Détection truecolor → choix de la dégradation monochrome (US-019 AC4).
pub fn supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}
