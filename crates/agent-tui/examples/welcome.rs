//! Aperçu de l'écran d'accueil (carte + logo pixel) :
//! `cargo run -p agent-tui --example welcome`. Rendu à plusieurs tailles via
//! `TestBackend`, sans terminal réel — pour eyeball avant le live.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_tui::{render, AppState};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

fn dump(state: &AppState, w: u16, h: u16, label: &str) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!(
        "\n── {label} {}",
        "─".repeat((w as usize).saturating_sub(label.len() + 4))
    );
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

/// Réémet le buffer en ANSI truecolor : montre le vrai dégradé du logo dans un
/// terminal 24-bit (sinon les demi-blocs bi-color paraissent uniformes). Les
/// codes ne sont émis que sur changement de couleur (sortie compacte).
fn dump_ansi(state: &AppState, w: u16, h: u16, label: &str) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!("\n── {label} · couleurs réelles ──");
    for y in 0..h {
        let mut line = String::new();
        let (mut cur_fg, mut cur_bg) = (Color::Reset, Color::Reset);
        line.push_str("\x1b[0m");
        for x in 0..w {
            let cell = &buf[(x, y)];
            if cell.fg != cur_fg {
                match cell.fg {
                    Color::Rgb(r, g, b) => line.push_str(&format!("\x1b[38;2;{r};{g};{b}m")),
                    _ => line.push_str("\x1b[39m"),
                }
                cur_fg = cell.fg;
            }
            if cell.bg != cur_bg {
                match cell.bg {
                    Color::Rgb(r, g, b) => line.push_str(&format!("\x1b[48;2;{r};{g};{b}m")),
                    _ => line.push_str("\x1b[49m"),
                }
                cur_bg = cell.bg;
            }
            line.push_str(cell.symbol());
        }
        line.push_str("\x1b[0m");
        println!("{line}");
    }
}

fn main() {
    let mut s = AppState::new("gpt-5.5", true);
    s.workspace = "numen".into();
    s.provider_connected = true;
    s.context_pct = Some(0);

    dump_ansi(&s, 88, 26, "accueil · 88×26");
    dump(&s, 64, 20, "accueil · 64×20 (formes)");
    dump(&s, 46, 14, "accueil compact (repli) · 46×14");
}
