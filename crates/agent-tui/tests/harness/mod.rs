//! Snapshot harness of the terminal rendering.
//!
//! Renders a full frame on `TestBackend` at a fixed terminal size and returns
//! the buffer as text. Three guarantees carried here rather than in
//! every test:
//!
//! 1. **Determinism** (AC2): the frame is rendered TWICE and both dumps
//!    must match. Any clock, randomness or environment read that would
//!    creep into the rendering path breaks the test at the source, instead of
//!    producing an unstable snapshot failing at random in CI.
//! 2. **No bare panic** (AC5): the rendering runs under `catch_unwind`; a
//!    panic becomes a test failure naming the offending state and the geometry.
//! 3. **No horizontal overflow** (US-006 AC4): no rendered line
//!    exceeds the terminal width, measured in terminal columns and not in
//!    bytes.
//!
//! Known and accepted limit: `TestBackend` does not reproduce the behavior
//! of a real PTY. These snapshots cover the RENDERING, not the terminal behavior
//! (raw mode, scrollback, synchronization sequences).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use agent_tui::custom_terminal::Terminal;
use agent_tui::render;
use agent_tui::state::AppState;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

/// Renders a frame of the `render` path (composer, status, menu, permission
/// dialog, legacy transcript) and returns the text dump to snapshot.
pub fn frame(label: &str, state: &AppState, width: u16, height: u16) -> String {
    capture(label, width, height, |terminal| {
        terminal.draw(|f| render(f, state)).unwrap();
    })
}

/// Renders the `ChatWidget` path used in inline-scrollback mode, following the
/// same two steps as the app loop: finalized cells are written above the
/// viewport, then the viewport is drawn, bottom-anchored, tall enough for its
/// content.
///
/// The dump concatenates what the terminal keeps (scrollback) and what it shows
/// (screen), separated by a marker. A cell appearing on both sides of that
/// marker is the double-render regression this path exists to prevent.
#[cfg(feature = "codex_tui_parity")]
pub fn chat_frame(
    label: &str,
    state: &AppState,
    chat: &mut agent_tui::ChatWidget,
    width: u16,
    height: u16,
) -> String {
    let mode = agent_tui::InsertHistoryMode::InlineScrollback;
    let pending = chat.surface_mut().drain_pending_insert(width, mode);
    let requested = agent_tui::parity_content_height(state, chat.surface(), width, height);
    let chat = &*chat;

    capture(label, width, height, |terminal| {
        // A fresh terminal hands the whole screen to the viewport; history takes
        // rows back from its top as it is written.
        terminal.set_viewport_area(Rect::new(0, 0, width, height));
        if let Some(insert) = pending.as_ref() {
            agent_tui::HistoryInserter::new(mode)
                .insert(terminal, insert)
                .unwrap();
        }
        terminal.anchor_viewport(requested).unwrap();
        terminal.draw(|frame| chat.render(frame, state)).unwrap();
    })
}

/// Core of the harness: double rendering, panic capture, width check.
fn capture(
    label: &str,
    width: u16,
    height: u16,
    draw: impl Fn(&mut Terminal<TestBackend>),
) -> String {
    let first = draw_once(label, width, height, &draw);
    let second = draw_once(label, width, height, &draw);
    assert_eq!(
        first, second,
        "rendu non déterministe pour `{label}` à {width}x{height} : deux rendus \
         du même état diffèrent (horloge, aléa ou environnement dans le chemin \
         de rendu)"
    );
    for (row, line) in first.lines().enumerate() {
        let rendered = line.width();
        assert!(
            rendered <= width as usize,
            "débordement horizontal pour `{label}` à {width}x{height} : la ligne \
             {row} occupe {rendered} colonnes\n{line}"
        );
    }
    first
}

/// A single rendering. The panic is turned into a failure naming the offending state
/// (AC5) instead of bubbling up as is from the guts of ratatui.
#[allow(
    clippy::panic,
    reason = "AC5 : une panique de rendu doit devenir un échec de test nommant \
              l'état fautif ; `resume_unwind` reperdrait ce contexte"
)]
fn draw_once(
    label: &str,
    width: u16,
    height: u16,
    draw: &impl Fn(&mut Terminal<TestBackend>),
) -> String {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut terminal = Terminal::full_screen(TestBackend::new(width, height), width, height);
        draw(&mut terminal);
        let backend = terminal.backend();
        let scrollback = backend.scrollback();
        if scrollback.area().height == 0 {
            dump(backend.buffer())
        } else {
            format!(
                "{}{SCROLLBACK_MARKER}\n{}",
                dump(scrollback),
                dump(backend.buffer())
            )
        }
    }));
    match outcome {
        Ok(rendered) => rendered,
        Err(payload) => {
            let cause = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panique sans message".to_string());
            panic!("panique de rendu sur `{label}` à {width}x{height} : {cause}")
        }
    }
}

/// Separates what the terminal keeps from what it currently shows.
const SCROLLBACK_MARKER: &str = "── scrollback above · screen below ──";

/// Ratatui buffer -> text. Trailing spaces are removed: they are invisible
/// in a snapshot diff and some editors rewrite them, which
/// would produce false regression positives.
fn dump(buffer: &Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
