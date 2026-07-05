//! Palette de rendu (US-032). Esthétique **monochrome + un accent bleu ciel orbital** : la
//! hiérarchie passe par le poids et la teinte, pas par la couleur. La couleur est
//! RÉSERVÉE au fonctionnel : les tons de diff (ajout/suppression) et `success`. En
//! l'absence de truecolor (AC4), tout dégrade en 16 couleurs / modifiers sans
//! perdre la distinction (la mise en page est inchangée).
//!
//! Extrait de `render.rs` pour centraliser les couleurs et garder le rendu pur.

use ratatui::style::{Color, Modifier, Style};

/// Palette : graphite + un accent bleu ciel + tons fonctionnels (erreur, diff,
/// succès). `truecolor` pilote la dégradation.
pub struct Theme {
    truecolor: bool,
}

impl Theme {
    pub fn new(truecolor: bool) -> Self {
        Self { truecolor }
    }

    /// Le terminal supporte-t-il le 24 bits ? (consommé par le rendu du logo, qui
    /// interpole une teinte continue uniquement en truecolor.)
    pub fn truecolor(&self) -> bool {
        self.truecolor
    }

    // ── Chrome monochrome + accent ──────────────────────────────────────────────

    pub fn fg(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xf2, 0xf0, 0xea))
        } else {
            Style::default()
        }
    }
    pub fn dim(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x8e, 0x94, 0x9e))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn faint(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x50, 0x57, 0x62))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn accent(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x6c, 0xcb, 0xff))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }
    pub fn error(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xff, 0x6b, 0x78))
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    }
    /// Fond de la ligne sélectionnée (menu de commandes) : voile bleu sombre en
    /// truecolor, vidéo inverse en 16 couleurs.
    pub fn selection(&self) -> Style {
        if self.truecolor {
            Style::default().bg(Color::Rgb(0x0f, 0x23, 0x34))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
    /// Trait horizontal du composer, visible sans enfermer l'input dans un bloc.
    pub fn composer_rule(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x2a, 0x2f, 0x37))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    /// Surbrillance d'un `/skill` inséré dans l'input : pastille bleu ciel sur fond sombre.
    pub fn skill_chip(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0x6c, 0xcb, 0xff))
                .bg(Color::Rgb(0x0f, 0x23, 0x34))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    // ── Tons FONCTIONNELS (couleur autorisée car porteuse de sens) ───────────────

    /// Succès / confirmation (ex. objectif atteint).
    pub fn success(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x78, 0xc9, 0x8a))
        } else {
            Style::default().fg(Color::Green)
        }
    }
    /// Ligne ajoutée d'un diff : fond vert sombre + texte clair (truecolor) ; en 16
    /// couleurs, vert simple (le signe `+` porte aussi le sens, pas que la couleur).
    pub fn diff_add(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xbd, 0xec, 0xc9))
                .bg(Color::Rgb(0x11, 0x28, 0x16))
        } else {
            Style::default().fg(Color::Green)
        }
    }
    /// Ligne supprimée d'un diff : fond rouge sombre + texte clair (truecolor).
    pub fn diff_remove(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xff, 0xc7, 0xcf))
                .bg(Color::Rgb(0x30, 0x13, 0x17))
        } else {
            Style::default().fg(Color::Red)
        }
    }
    /// Segment ajouté MOT-À-MOT (emphase intra-ligne) : fond vert saturé.
    pub fn diff_add_word(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xf5, 0xff, 0xf7))
                .bg(Color::Rgb(0x2a, 0x6a, 0x39))
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::REVERSED)
        }
    }
    /// Segment supprimé MOT-À-MOT : fond rouge saturé.
    pub fn diff_remove_word(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0xff, 0xf0, 0xf2))
                .bg(Color::Rgb(0x7b, 0x2a, 0x35))
        } else {
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::REVERSED)
        }
    }
}
