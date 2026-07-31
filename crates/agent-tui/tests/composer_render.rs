//! Rendering contracts of the multi-line composer (US-010,
//! `tasks/prd-harness-parity.md`).
//!
//! What snapshots cannot prove: the SCREEN position of the cursor
//! (it does not appear in the buffer dump) and the rendering time. Both
//! are measured here on the real `render` path, through `TestBackend`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use agent_core::AgentEvent;
use agent_tui::render;
use agent_tui::state::AppState;
use agent_tui::custom_terminal::Terminal;
use ratatui::backend::TestBackend;

/// Renders a frame and returns the cursor position reported by the backend.
fn cursor_after_render(state: &AppState, width: u16, height: u16) -> (u16, u16) {
    let mut terminal = Terminal::full_screen(TestBackend::new(width, height), width, height);
    terminal.draw(|f| render(f, state)).unwrap();
    let position = terminal.get_cursor_position().unwrap();
    (position.x, position.y)
}

fn state_with(input: &str) -> AppState {
    let mut s = AppState::new("gpt-5", true);
    s.workspace = "pyxis".into();
    s.set_input(input.to_string());
    s
}

/// AC4: on a folded line, the screen column follows the logical position.
#[test]
fn cursor_follows_logical_position_across_a_wrap() {
    // Width 40 -> text area of 38 columns, gutter of 2. "abcdefghij" x6
    // is 66 columns: cut after the last space fitting in 38, that is at
    // byte 33, hence two visual lines of 33 columns.
    let long = "abcdefghij ".repeat(6);
    let mut s = state_with(&long);

    // Cursor at the very start: first text line, right after the gutter.
    s.cursor = 0;
    let (x0, y0) = cursor_after_render(&s, 40, 24);
    assert_eq!(x0, 2);

    // Cursor on the fold point: it shows at the START of the continuation,
    // where the next typed character will appear.
    s.cursor = 33;
    let (x1, y1) = cursor_after_render(&s, 40, 24);
    assert_eq!(y1, y0 + 1, "le curseur descend d'une ligne visuelle");
    assert_eq!(x1, 2);

    // Cursor at the end of the input: same visual line, column = width of the
    // walked segment + gutter.
    s.cursor = long.len();
    let (x2, y2) = cursor_after_render(&s, 40, 24);
    assert_eq!(y2, y1);
    assert_eq!(x2, 2 + 33);
}

/// AC4 on an explicit multi-line input: line 3, column 5.
#[test]
fn cursor_tracks_explicit_newlines() {
    let mut s = state_with("aaa\nbbb\ncccccccc");
    s.cursor = "aaa\nbbb\nccccc".len();
    let (x, y) = cursor_after_render(&s, 80, 24);
    let (_, y_first) = {
        let mut t = s.clone();
        t.cursor = 0;
        cursor_after_render(&t, 80, 24)
    };
    assert_eq!(y, y_first + 2);
    assert_eq!(x, 2 + 5);
}

/// AC3: past the height cap, the area scrolls and the cursor line
/// stays inside the frame.
#[test]
fn cursor_stays_visible_beyond_the_height_cap() {
    let input = (1..=30)
        .map(|i| format!("ligne {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut s = state_with(&input);

    // Cursor at the end: last line of the composer, above the status line.
    let (_, y_last) = cursor_after_render(&s, 80, 24);
    assert!(y_last < 24, "curseur hors écran");

    // Cursor at the start: the area scrolls back, the cursor stays visible.
    s.cursor = 0;
    let (_, y_first) = cursor_after_render(&s, 80, 24);
    assert!(y_first < 24);
    assert!(
        y_first < y_last,
        "la fenêtre doit suivre le curseur ({y_first} vs {y_last})"
    );
}

/// AC5: P95 rendering under 16 ms with a ten-line composer and a loaded
/// transcript. Deliberately wide margin: the goal is to detect a collapse
/// (quadratic recomputation, cache bypassed), not to measure the machine.
#[test]
fn p95_frame_time_under_16ms_with_a_ten_line_composer() {
    let mut s = AppState::new("gpt-5", true);
    s.workspace = "pyxis".into();
    for i in 0..250 {
        s.push_user(format!("question {i} sur la boucle d'agent"));
        s.apply(&AgentEvent::Text(format!(
            "Réponse {i}.\n\n```rust\nfn main() {{ println!(\"{i}\"); }}\n```\n"
        )));
        s.apply(&AgentEvent::EndTurn);
    }
    assert!(s.blocks.len() >= 500);
    s.set_input(
        (1..=10)
            .map(|i| format!("ligne {i} d'un prompt structuré en paragraphes"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let mut terminal = Terminal::full_screen(TestBackend::new(200, 50), 200, 50);
    let mut samples: Vec<Duration> = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        terminal.draw(|f| render(f, &s)).unwrap();
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    let p95 = samples[94];
    assert!(
        p95 < Duration::from_millis(16),
        "rendu P95 = {p95:?} (budget 16 ms) sur {} frames",
        samples.len()
    );
}
