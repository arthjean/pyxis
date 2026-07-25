//! Contrats de rendu du composer multi-ligne (US-010,
//! `tasks/prd-harness-parity.md`).
//!
//! Ce que les snapshots ne peuvent pas prouver : la position ÉCRAN du curseur
//! (elle n'apparaît pas dans le dump du buffer) et le temps de rendu. Les deux
//! sont mesurés ici sur le chemin `render` réel, via `TestBackend`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use agent_core::AgentEvent;
use agent_tui::render;
use agent_tui::state::AppState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Rend une frame et retourne la position du curseur rapportée par le backend.
fn cursor_after_render(state: &AppState, width: u16, height: u16) -> (u16, u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
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

/// AC4 : sur une ligne repliée, la colonne écran suit la position logique.
#[test]
fn cursor_follows_logical_position_across_a_wrap() {
    // Largeur 40 → zone texte de 38 colonnes, gouttière de 2. « abcdefghij » x6
    // fait 66 colonnes : coupure après le dernier espace tenant dans 38, soit à
    // l'octet 33, d'où deux lignes visuelles de 33 colonnes.
    let long = "abcdefghij ".repeat(6);
    let mut s = state_with(&long);

    // Curseur au tout début : première ligne de texte, juste après la gouttière.
    s.cursor = 0;
    let (x0, y0) = cursor_after_render(&s, 40, 24);
    assert_eq!(x0, 2);

    // Curseur sur le point de repli : il s'affiche au DÉBUT de la continuation,
    // là où le prochain caractère saisi apparaîtra.
    s.cursor = 33;
    let (x1, y1) = cursor_after_render(&s, 40, 24);
    assert_eq!(y1, y0 + 1, "le curseur descend d'une ligne visuelle");
    assert_eq!(x1, 2);

    // Curseur en fin de saisie : même ligne visuelle, colonne = largeur du
    // segment parcouru + gouttière.
    s.cursor = long.len();
    let (x2, y2) = cursor_after_render(&s, 40, 24);
    assert_eq!(y2, y1);
    assert_eq!(x2, 2 + 33);
}

/// AC4 sur une saisie multi-ligne explicite : ligne 3, colonne 5.
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

/// AC3 : au-delà du plafond de hauteur, la zone défile et la ligne du curseur
/// reste dans le cadre.
#[test]
fn cursor_stays_visible_beyond_the_height_cap() {
    let input = (1..=30)
        .map(|i| format!("ligne {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut s = state_with(&input);

    // Curseur en fin : dernière ligne du composer, au-dessus de la ligne de statut.
    let (_, y_last) = cursor_after_render(&s, 80, 24);
    assert!(y_last < 24, "curseur hors écran");

    // Curseur au début : la zone remonte, le curseur reste visible.
    s.cursor = 0;
    let (_, y_first) = cursor_after_render(&s, 80, 24);
    assert!(y_first < 24);
    assert!(
        y_first < y_last,
        "la fenêtre doit suivre le curseur ({y_first} vs {y_last})"
    );
}

/// AC5 : rendu P95 sous 16 ms avec un composer de dix lignes et un transcript
/// chargé. Marge volontairement large : le but est de détecter un effondrement
/// (recalcul quadratique, cache contourné), pas de mesurer la machine.
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

    let mut terminal = Terminal::new(TestBackend::new(200, 50)).unwrap();
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
