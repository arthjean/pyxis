//! Terminal setup/teardown and viewport anchoring. Isolated here so that
//! rendering (`render.rs`) stays pure and testable without a real terminal.
//!
//! The parity path keeps the native terminal scrollback: no alternate screen,
//! and a viewport that is only as tall as what the renderer actually owns
//! (active cell plus bottom pane). Everything above it is finalized history the
//! terminal keeps for good. The legacy path still uses the alternate screen and
//! draws the whole transcript itself.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::MoveTo;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
#[cfg(not(feature = "codex_tui_parity"))]
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
#[cfg(not(feature = "codex_tui_parity"))]
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::custom_terminal::{Frame, Terminal};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Is the terminal currently in interactive mode? Process-wide, because raw mode
/// and the alternate screen are: a panic hook has no `Tui` handle to consult and
/// must still know whether there is anything to restore (US-020 AC1).
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

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
    if let Err(e) = execute!(out, EnableBracketedPaste) {
        let _ = disable_raw_mode();
        return Err(e);
    }

    match Terminal::new(CrosstermBackend::new(out)) {
        Ok(tui) => {
            let mut tui = tui;
            // Legacy owns the whole (alternate) screen; parity wipes the visible
            // screen so the session starts on a clean one instead of below
            // whatever the shell left there (build output, a previous command).
            #[cfg(not(feature = "codex_tui_parity"))]
            {
                let size = tui.size()?;
                tui.set_viewport_area(Rect::new(0, 0, size.width, size.height));
                tui.clear_screen()?;
            }
            #[cfg(feature = "codex_tui_parity")]
            if let Err(e) = clear(&mut tui) {
                let _ = execute!(io::stdout(), DisableBracketedPaste);
                let _ = disable_raw_mode();
                return Err(e);
            }
            #[cfg(feature = "codex_tui_parity")]
            crate::debug_log::log(&format!("enter: viewport={:?}", tui.viewport_area));
            ACTIVE.store(true, Ordering::SeqCst);
            Ok(tui)
        }
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

/// Draws one frame into a viewport of `height` rows anchored at the bottom of
/// the screen.
///
/// Growing the viewport steals rows from the history above it, which is why the
/// rows are scrolled up (into the scrollback) rather than overwritten. Shrinking
/// it leaves rows behind that nothing else repaints, so the area from the
/// highest row either viewport touched is cleared before the frame is drawn.
pub fn draw(tui: &mut Tui, height: u16, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
    tui.anchor_viewport(height)?;
    tui.draw(render)
}

/// Debounce before reflowing. Dragging a window edge emits a resize per pixel
/// column; reflowing on each one would rewrite the transcript dozens of times.
pub const REFLOW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(75);

/// Frees the history rows this session still owns on screen, so the transcript
/// can be rewritten at the current width. Returns how many rows are available.
///
/// Deliberately not `ESC[3J`: purging the scrollback would also erase what the
/// user had in their terminal before Pyxis started. Rows that already scrolled
/// out keep their old wrapping, exactly like the output of any other program.
pub fn clear_for_reflow(tui: &mut Tui) -> io::Result<u16> {
    tui.clear_owned_history()
}

/// Wipes the visible screen and anchors an empty viewport at its top, the state
/// a session starts and ends on.
///
/// Anchoring at the top rather than at the last row matters on the way in: the
/// first frame then grows into an empty screen instead of scrolling as many
/// blank rows into the scrollback as it needs.
///
/// Deliberately not `ESC[3J` here either: the screen is cleared, the scrollback
/// is not purged, so whatever the user had before Pyxis started stays one scroll
/// away.
pub fn clear(tui: &mut Tui) -> io::Result<()> {
    write_clear_sequence(tui.backend_mut())?;
    let size = tui.size()?;
    tui.set_viewport_area(Rect::new(0, 0, size.width, 0));
    tui.invalidate_viewport();
    Ok(())
}

/// The escape sequences of `clear`, isolated from the real terminal so a test
/// can read what a session start emits.
pub fn write_clear_sequence(out: &mut impl Write) -> io::Result<()> {
    execute!(out, Clear(ClearType::All), MoveTo(0, 0))
}

/// Wipes the visible screen AND purges the scrollback, then anchors an empty
/// viewport at its top: the state a freshly started process shows, minus the
/// rows this session had written.
///
/// This is what `/clear` asks for, and the one place where purging is right:
/// the user asked for a clean terminal, so the transcript is dropped for good
/// instead of staying one scroll away. Everywhere else (session start, reflow)
/// the scrollback is left alone.
pub fn clear_including_scrollback(tui: &mut Tui) -> io::Result<()> {
    write_clear_scrollback_sequence(tui.backend_mut())?;
    let size = tui.size()?;
    tui.set_viewport_area(Rect::new(0, 0, size.width, 0));
    tui.invalidate_viewport();
    Ok(())
}

/// The escape sequence of `clear_including_scrollback`, isolated from the real
/// terminal so a test can read it.
///
/// One write rather than a chain of crossterm commands: some terminals
/// (Terminal.app, Warp) do not reliably drop their scrollback when the clear and
/// the purge reach them separately. Scroll region reset, attributes reset,
/// cursor home, screen cleared, scrollback purged, cursor home again.
pub fn write_clear_scrollback_sequence(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
    out.flush()
}

/// Restores the terminal from OUTSIDE the normal exit path: panic hook, signal,
/// any place holding no `Tui`. Best effort and infallible by construction, because
/// its caller is already handling a failure (US-020 AC1). A no-op when the
/// interactive mode is not active, so a headless panic emits no escape sequence.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    let _ = write_restore_sequence(&mut out);
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// The escape sequences of `restore`, isolated from the real terminal so a test
/// can read what a panic would emit.
pub fn write_restore_sequence(out: &mut impl Write) -> io::Result<()> {
    #[cfg(not(feature = "codex_tui_parity"))]
    {
        execute!(
            out,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        )
    }
    #[cfg(feature = "codex_tui_parity")]
    {
        execute!(out, DisableBracketedPaste, crossterm::cursor::Show)
    }
}

/// Restores the terminal (to be called on exit, including on error).
pub fn leave(tui: &mut Tui) -> io::Result<()> {
    ACTIVE.store(false, Ordering::SeqCst);
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

#[cfg(test)]
mod sequence_tests {
    use super::*;

    /// US-020 AC1: the sequence that a panic emits before its message actually
    /// leaves the interactive mode. Written into a buffer: the test needs no
    /// terminal, which is exactly the point of isolating the seam.
    #[test]
    fn the_restore_sequence_leaves_interactive_mode() {
        let mut out: Vec<u8> = Vec::new();
        write_restore_sequence(&mut out).expect("écriture en mémoire");
        let rendered = String::from_utf8_lossy(&out);
        assert!(!rendered.is_empty(), "aucune séquence émise");
        // Bracketed paste off + cursor shown, whatever the rendering path.
        assert!(
            rendered.contains("?2004l"),
            "collage entre crochets: {rendered:?}"
        );
        assert!(rendered.contains("?25h"), "curseur masqué: {rendered:?}");
    }

    /// The screen a session starts on is wiped, but the scrollback is not
    /// purged: what the user had in the terminal before Pyxis stays reachable.
    #[test]
    fn the_clear_sequence_wipes_the_screen_without_purging_the_scrollback() {
        let mut out: Vec<u8> = Vec::new();
        write_clear_sequence(&mut out).expect("écriture en mémoire");
        let rendered = String::from_utf8_lossy(&out);
        assert!(rendered.contains("[2J"), "écran non effacé: {rendered:?}");
        assert!(
            rendered.contains("[1;1H"),
            "curseur non replacé: {rendered:?}"
        );
        assert!(!rendered.contains("[3J"), "défilement purgé: {rendered:?}");
    }

    /// `/clear` is the opposite promise: the screen AND the scrollback go, so
    /// the session leaves nothing behind to scroll back into.
    #[test]
    fn the_clear_scrollback_sequence_purges_what_the_session_wrote() {
        let mut out: Vec<u8> = Vec::new();
        write_clear_scrollback_sequence(&mut out).expect("écriture en mémoire");
        let rendered = String::from_utf8_lossy(&out);
        assert_eq!(
            rendered, "\u{1b}[r\u{1b}[0m\u{1b}[H\u{1b}[2J\u{1b}[3J\u{1b}[H",
            "séquence inattendue: {rendered:?}"
        );
    }

    /// Nothing is active outside an interactive session: a headless panic must
    /// not write escape sequences into a redirected output.
    #[test]
    fn restore_is_a_no_op_without_an_interactive_session() {
        assert!(!is_active());
        restore();
        assert!(!is_active());
    }
}

#[cfg(test)]
mod viewport_tests {
    use crate::custom_terminal::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect, Size};
    use ratatui::widgets::Paragraph;

    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::with_geometry(
            TestBackend::new(width, height),
            Size::new(width, height),
            Position { x: 0, y: 0 },
        )
    }

    fn draw_at<B: ratatui::backend::Backend>(
        tui: &mut Terminal<B>,
        height: u16,
    ) -> std::io::Result<()> {
        tui.anchor_viewport(height)?;
        tui.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Paragraph::new("x"), area);
        })
    }

    #[test]
    fn a_growing_viewport_stays_anchored_at_the_bottom() {
        let mut tui = terminal(10, 8);
        tui.set_viewport_area(Rect::new(0, 7, 10, 1));

        draw_at(&mut tui, 3).expect("rendu");

        assert_eq!(tui.viewport_area, Rect::new(0, 5, 10, 3));
    }

    /// Content shorter than the viewport does not shrink it: the surplus becomes
    /// empty space above the composer, which stays on the last row. Only history
    /// insertion takes rows back, and it takes them from the top.
    #[test]
    fn shorter_content_leaves_the_viewport_anchored_at_the_bottom() {
        let mut tui = terminal(10, 8);
        tui.set_viewport_area(Rect::new(0, 3, 10, 5));

        draw_at(&mut tui, 2).expect("rendu");

        assert_eq!(tui.viewport_area, Rect::new(0, 3, 10, 5));
        assert_eq!(tui.viewport_area.bottom(), 8);
    }

    /// A viewport that lost rows to history keeps its bottom edge: the composer
    /// never drifts up the screen.
    #[test]
    fn the_viewport_bottom_never_leaves_the_last_row() {
        let mut tui = terminal(10, 8);
        tui.set_viewport_area(Rect::new(0, 6, 10, 2));

        draw_at(&mut tui, 1).expect("rendu");

        assert_eq!(tui.viewport_area.bottom(), 8);
        assert_eq!(tui.viewport_area.y, 6, "aucune rangée reprise sans besoin");
    }

    /// A screen that loses rows scrolls its content up, the viewport included.
    /// Re-anchoring on the stale position would leave the rows the previous
    /// frame wrote above the new one, as a ghost of the card.
    #[test]
    fn a_shorter_screen_re_anchors_the_viewport_on_the_rows_that_moved() {
        let mut tui = terminal(10, 20);
        tui.set_viewport_area(Rect::new(0, 8, 10, 12));
        draw_at(&mut tui, 12).expect("rendu");

        tui.backend_mut().resize(10, 14);
        draw_at(&mut tui, 6).expect("rendu après réduction");

        // The twelve rows moved from 8..20 to 2..14; the viewport owns them
        // again instead of starting at row 8 and leaving six behind.
        assert_eq!(tui.viewport_area, Rect::new(0, 2, 10, 12));
    }

    #[test]
    fn the_viewport_never_exceeds_the_screen() {
        let mut tui = terminal(10, 4);
        tui.set_viewport_area(Rect::new(0, 3, 10, 1));

        draw_at(&mut tui, 40).expect("rendu");

        assert_eq!(tui.viewport_area, Rect::new(0, 0, 10, 4));
    }
}
