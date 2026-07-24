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

/// Vrai quand le viewport inline ne couvre plus toute la hauteur du terminal.
///
/// `Viewport::Inline(h)` fige `h` à la construction : ratatui clampe ensuite à
/// `min(hauteur_écran, h)`. Un terminal RÉTRÉCI est donc suivi correctement, mais
/// un terminal AGRANDI garde l'ancienne hauteur, et tout le rendu (transcript,
/// composer, barre de statut) reste enfermé dans cette zone périmée.
#[cfg(feature = "codex_tui_parity")]
pub fn inline_viewport_stale(viewport_height: u16, screen_height: u16) -> bool {
    viewport_height < screen_height
}

/// Réaligne le viewport inline sur la hauteur courante du terminal, en repartant
/// d'un `Terminal` neuf : ratatui n'expose aucun moyen de changer la hauteur d'un
/// `Viewport::Inline`. Renvoie `true` si la reconstruction a eu lieu.
///
/// Le scrollback déjà émis par `insert_before` n'est pas touché : on efface l'écran
/// visible et le prochain `draw` repeint depuis l'état, comme au démarrage.
///
/// La construction interroge la position du curseur ; l'appelant DOIT donc garantir
/// qu'aucun `crossterm::event::read()` bloquant ne tourne en parallèle, sans quoi la
/// réponse du terminal reste captive de ce lecteur et la requête expire.
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
    crate::debug_log::log(&format!("sync: rebuilt viewport={:?}", tui.get_frame().area()));
    Ok(true)
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

    /// Verrouille la contrainte ratatui qui impose la reconstruction du terminal.
    /// Si ce test casse (hauteur suivie automatiquement), `sync_inline_viewport`
    /// devient inutile et peut disparaître.
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
