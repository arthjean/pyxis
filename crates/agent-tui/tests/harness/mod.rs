//! Harness de snapshot du rendu terminal (US-005, `tasks/prd-harness-parity.md`).
//!
//! Rend une frame complète sur `TestBackend` à taille de terminal fixe et rend
//! le buffer sous forme texte. Trois garanties portées ici plutôt que dans
//! chaque test :
//!
//! 1. **Déterminisme** (AC2) : la frame est rendue DEUX fois et les deux dumps
//!    doivent coïncider. Toute lecture d'horloge, d'aléa ou d'environnement qui
//!    s'inviterait dans le chemin de rendu casse le test à la source, au lieu de
//!    produire un snapshot instable qui échouerait au hasard en CI.
//! 2. **Pas de panique nue** (AC5) : le rendu tourne sous `catch_unwind` ; une
//!    panique devient un échec de test nommant l'état fautif et la géométrie.
//! 3. **Pas de débordement horizontal** (US-006 AC4) : aucune ligne rendue ne
//!    dépasse la largeur du terminal, mesurée en colonnes terminal et non en
//!    octets.
//!
//! Limite connue et assumée : `TestBackend` ne reproduit pas le comportement
//! d'un PTY réel. Ces snapshots couvrent le RENDU, pas le comportement terminal
//! (raw mode, scrollback, séquences de synchronisation).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use agent_tui::render;
use agent_tui::state::AppState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

/// Rend une frame du chemin `render` (composer, statut, menu, dialog de
/// permission, transcript legacy) et retourne le dump texte à snapshotter.
pub fn frame(label: &str, state: &AppState, width: u16, height: u16) -> String {
    capture(label, width, height, |terminal| {
        terminal.draw(|f| render(f, state)).unwrap();
    })
}

/// Rend une frame du chemin `render_parity` (surface d'historique réellement
/// utilisée en mode inline-scrollback).
#[cfg(feature = "codex_tui_parity")]
pub fn frame_parity(
    label: &str,
    state: &AppState,
    surface: &agent_tui::ChatSurface,
    width: u16,
    height: u16,
) -> String {
    capture(label, width, height, |terminal| {
        terminal
            .draw(|f| agent_tui::render_parity(f, state, surface))
            .unwrap();
    })
}

/// Cœur du harness : double rendu, capture de panique, contrôle de largeur.
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

/// Un rendu isolé. La panique est convertie en échec nommant l'état fautif
/// (AC5) au lieu de remonter telle quelle depuis les entrailles de ratatui.
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
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        draw(&mut terminal);
        dump(terminal.backend().buffer())
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

/// Buffer ratatui → texte. Les espaces de fin sont retirés : ils sont invisibles
/// dans un diff de snapshot et certains éditeurs les réécrivent, ce qui
/// produirait des faux positifs de régression.
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
