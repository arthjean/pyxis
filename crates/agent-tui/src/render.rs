//! Rendu Ratatui (US-019). Esthétique : **monochrome + un accent**, épurée,
//! aucune bordure lourde. Hiérarchie par poids/teinte et espace négatif, pas par
//! couleur. Signature visuelle : une gouttière `▌` qui s'allume (accent) sur le
//! tour assistant en cours de stream, et se calme (faint) une fois fini.
//!
//! `render` est PUR → testable via `TestBackend`. La dégradation sans truecolor
//! (AC4) remplace l'accent par du gras ; la mise en page est inchangée.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as Boundary, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{AppState, Block, DiffKind, MenuItem, PermissionPrompt, Status, COMMANDS};

/// Palette : grayscale + un accent (teal) + un ton d'erreur (rouge muté). En
/// l'absence de truecolor, tout passe en gras/dim 16 couleurs (AC4).
pub struct Theme {
    truecolor: bool,
}

impl Theme {
    pub fn new(truecolor: bool) -> Self {
        Self { truecolor }
    }

    pub fn fg(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xe4, 0xe4, 0xe4))
        } else {
            Style::default()
        }
    }
    pub fn dim(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x8a, 0x8a, 0x8a))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn faint(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x4e, 0x4e, 0x4e))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
    pub fn accent(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0x6f, 0xd0, 0xc8))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }
    pub fn error(&self) -> Style {
        if self.truecolor {
            Style::default().fg(Color::Rgb(0xd0, 0x6a, 0x6a))
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    }
    /// Fond de la ligne sélectionnée (menu de commandes) : un voile teal sombre
    /// en truecolor, vidéo inverse en dégradé 16 couleurs.
    pub fn selection(&self) -> Style {
        if self.truecolor {
            Style::default().bg(Color::Rgb(0x1c, 0x2e, 0x2c))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
    /// Surbrillance d'un `/skill` inséré dans l'input : pastille teal sur fond
    /// sombre (vidéo inverse + gras en 16 couleurs).
    pub fn skill_chip(&self) -> Style {
        if self.truecolor {
            Style::default()
                .fg(Color::Rgb(0x6f, 0xd0, 0xc8))
                .bg(Color::Rgb(0x1c, 0x2e, 0x2c))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }
}

const INDENT: &str = "  ";
/// Zone de saisie : box bordée (3 lignes) + ligne de statut (1).
const INPUT_HEIGHT: u16 = 4;

/// Rendu complet d'une frame.
pub fn render(frame: &mut Frame, state: &AppState) {
    let theme = Theme::new(state.truecolor);
    let area = frame.area();

    // En bas : soit le dialog de permission, soit (status + input).
    let bottom_height = match &state.pending {
        Some(p) => permission_height(p, area.width),
        None => INPUT_HEIGHT,
    };
    // Menu de commandes slash : popup intercalé entre transcript et input (jamais
    // pendant un dialog de permission). +1 ligne pour le rappel des raccourcis.
    let matches = state.menu_items();
    let menu = state.pending.is_none() && state.menu_open();
    let menu_height = if menu { matches.len() as u16 + 1 } else { 0 };

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(menu_height),
        Constraint::Length(bottom_height),
    ])
    .split(area);

    // Transcript vide → écran d'accueil (carte + logo pixel), sinon le fil normal.
    if state.is_welcome() {
        render_welcome(frame, chunks[0], state, &theme);
    } else {
        render_transcript(frame, chunks[0], state, &theme);
    }
    if menu {
        render_command_menu(frame, chunks[1], state, &theme, &matches);
    }
    match &state.pending {
        Some(prompt) => render_permission(frame, chunks[2], prompt, &theme),
        None => render_input(frame, chunks[2], state, &theme),
    }
}

/// Logo de Numen : une **sphère de Dyson** minimaliste. Le numen est la
/// présence/volonté qui canalise une puissance brute pour la rendre utile : ici
/// un cœur stellaire net cerné de deux anneaux de collecteurs (avec brèches,
/// l'essaim en assemblage). Rendu en **points braille tramés** (stippling, façon
/// pixel-dust), monochrome ; l'accent teal reste réservé à l'UI. Champ continu
/// (résolution-indépendant), pas un bitmap figé.
const LOGO_COLS: usize = 20;
const LOGO_ROWS: usize = 10;
/// Épaisseur des anneaux / taille du cœur / densité des points (réglage « 11d »).
const LOGO_LINE_W: f32 = 0.11;
const LOGO_CORE_W: f32 = 0.15;
const LOGO_GAMMA: f32 = 0.7;

/// Matrice de Bayer 4×4 (tramage ordonné) : convertit l'intensité du champ en
/// densité de points (le « plus ou moins resserré »).
const LOGO_BAYER: [[f32; 4]; 4] = [
    [0.0, 8.0, 2.0, 10.0],
    [12.0, 4.0, 14.0, 6.0],
    [3.0, 11.0, 1.0, 9.0],
    [15.0, 7.0, 13.0, 5.0],
];

/// Disposition des 8 points d'une cellule braille → bit (base U+2800).
const LOGO_DOTS: [(usize, usize, u8); 8] = [
    (0, 0, 0x01),
    (0, 1, 0x02),
    (0, 2, 0x04),
    (0, 3, 0x40),
    (1, 0, 0x08),
    (1, 1, 0x10),
    (1, 2, 0x20),
    (1, 3, 0x80),
];

/// Champ continu de la sphère de Dyson en coordonnées normalisées nx,ny ∈ [-1,1]
/// (rayon 1 = bord) : cœur stellaire gaussien + deux anneaux fins inclinés, avec
/// une brèche chacun. Retourne une intensité 0.0 (vide) .. 1.0 (cœur).
fn logo_field(nx: f32, ny: f32) -> f32 {
    use std::f32::consts::TAU;
    let rn = (nx * nx + ny * ny).sqrt();
    let core = (-(rn / LOGO_CORE_W).powi(2)).exp();
    // (inclinaison, ratio petit axe, début de brèche, fin de brèche) en radians.
    let rings = [
        (0.50_f32, 0.30_f32, 1.1_f32, 2.3_f32),
        (-0.62, 0.26, 4.0, 5.0),
    ];
    let mut ring = 0.0_f32;
    for (tilt, br, gap_start, gap_end) in rings {
        let (ct, st) = (tilt.cos(), tilt.sin());
        let u = nx * ct + ny * st;
        let v = -nx * st + ny * ct;
        let e = ((u / 0.88).powi(2) + (v / br).powi(2)).sqrt();
        let line = (-(((e - 1.0) / LOGO_LINE_W).powi(2))).exp();
        let phi = v.atan2(u).rem_euclid(TAU);
        if !(phi > gap_start && phi < gap_end) {
            ring = ring.max(line);
        }
    }
    core.max(ring * 0.9)
}

/// Rend le champ du logo en points braille tramés (2×4 sous-points par cellule).
/// La densité suit l'intensité boostée par `LOGO_GAMMA` (< 1 = plus de points,
/// fond vrai préservé). Monochrome : gris du thème selon le pic de la cellule ;
/// sans truecolor, repli sur `fg`.
fn logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    let (cols, rows) = (LOGO_COLS, LOGO_ROWS);
    let (sw, sh) = (cols * 2, rows * 4); // sous-grille carrée (cols = 2·rows)
    let scale = 1.05_f32; // léger jeu autour du logo
    let mut lines = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for cx in 0..cols {
            let mut bits = 0u8;
            let mut peak = 0.0_f32;
            for (ddx, ddy, bit) in LOGO_DOTS {
                let (sx, sy) = (cx * 2 + ddx, cy * 4 + ddy);
                let nx = (sx as f32 + 0.5 - sw as f32 / 2.0) / (sw as f32 / 2.0) * scale;
                let ny = (sy as f32 + 0.5 - sh as f32 / 2.0) / (sh as f32 / 2.0) * scale;
                let inten = logo_field(nx, ny).powf(LOGO_GAMMA);
                let thr = (LOGO_BAYER[sy & 3][sx & 3] + 0.5) / 16.0;
                if inten > thr {
                    bits |= bit;
                    peak = peak.max(inten);
                }
            }
            if bits == 0 {
                spans.push(Span::raw(" "));
                continue;
            }
            let ch = char::from_u32(0x2800 + bits as u32)
                .unwrap_or(' ')
                .to_string();
            let style = if theme.truecolor {
                // Gris dans une bande médiane (ni trop sombre, ni blanc pur).
                let v = (0x6a as f32 + peak.clamp(0.0, 1.0) * (0xde - 0x6a) as f32) as u8;
                Style::default().fg(Color::Rgb(v, v, v))
            } else {
                theme.fg()
            };
            spans.push(Span::styled(ch, style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Rect centré dans `area`, clampé à ses bornes (jamais plus grand qu'`area`).
fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Écran d'accueil (hero façon Grok) : une carte centrée, logo braille (sphère
/// de Dyson, monochrome) à gauche, identité + raccourcis à droite. Affiché tant
/// qu'aucune conversation n'a démarré (`AppState::is_welcome`) ; l'input reste
/// rendu dessous, inchangé. Repli compact (sans logo ni bordure) si le terminal
/// est trop étroit pour la carte complète.
fn render_welcome(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Pas de transcript à scroller en accueil.
    state.scroll_max.set(0);

    let logo = logo_lines(theme);
    let logo_w = logo.iter().map(|l| l.width()).max().unwrap_or(0) as u16;

    // Colonne de droite : identité, méta (modèle/workspace/provider), raccourcis.
    let mut info: Vec<Line> = vec![
        Line::from(Span::styled(
            "NUMEN",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "ton agent de code en terminal",
            theme.dim().add_modifier(Modifier::ITALIC),
        )),
        Line::default(),
    ];
    let mut meta = vec![
        Span::styled("◆ ", theme.faint()),
        Span::styled(state.model.clone(), theme.dim()),
    ];
    if !state.workspace.is_empty() {
        meta.push(Span::styled("  ·  ", theme.faint()));
        meta.push(Span::styled(format!("{}/", state.workspace), theme.dim()));
    }
    info.push(Line::from(meta));
    if state.provider_connected {
        info.push(Line::from(vec![
            Span::styled("✓ codex", theme.accent()),
            Span::styled("  abonnement ChatGPT", theme.dim()),
        ]));
    }
    info.push(Line::default());
    info.push(Line::from(vec![
        Span::styled("/help", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/models", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/goal", theme.accent()),
    ]));
    info.push(Line::from(Span::styled(
        "⌃C quitter   ·   ↑ historique",
        theme.faint(),
    )));

    let info_w = info.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let gap: u16 = 3; // colonne de respiration entre logo et texte
    let pad: u16 = 2; // marge interne horizontale (de part et d'autre)
    let inner_w = logo_w + gap + info_w;
    let inner_h = logo.len().max(info.len()) as u16;
    let card_w = inner_w + pad * 2 + 2; // + 2 bordures
    let card_h = inner_h + 4; // 2 lignes de marge (haut/bas) + 2 bordures

    // Terminal trop petit pour la carte complète → repli compact.
    if area.width < card_w || area.height < card_h {
        render_welcome_compact(frame, area, &info);
        return;
    }

    let rect = centered_rect(area, card_w, card_h);
    let frame_block = Boundary::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.faint());
    let content = frame_block.inner(rect);
    frame.render_widget(frame_block, rect);

    // Compose chaque ligne : logo (gauche) + gap + info (droite), les deux blocs
    // centrés verticalement dans `inner_h`.
    let logo_off = (inner_h - logo.len() as u16) / 2;
    let info_off = (inner_h - info.len() as u16) / 2;
    let mut rows: Vec<Line> = Vec::with_capacity(inner_h as usize);
    for i in 0..inner_h {
        let mut spans: Vec<Span> = Vec::new();
        match i.checked_sub(logo_off).map(|j| logo.get(j as usize)) {
            Some(Some(line)) => spans.extend(line.spans.iter().cloned()),
            _ => spans.push(Span::raw(" ".repeat(logo_w as usize))),
        }
        spans.push(Span::raw(" ".repeat(gap as usize)));
        if let Some(Some(line)) = i.checked_sub(info_off).map(|j| info.get(j as usize)) {
            spans.extend(line.spans.iter().cloned());
        }
        rows.push(Line::from(spans));
    }

    // 1 ligne de marge en haut, `pad` colonnes à gauche, à l'intérieur du cadre.
    let body = Rect {
        x: content.x + pad,
        y: content.y + 1,
        width: content.width.saturating_sub(pad),
        height: content.height.saturating_sub(1),
    };
    frame.render_widget(Paragraph::new(rows), body);
}

/// Repli de l'accueil pour terminal étroit : le bloc d'identité seul, centré,
/// sans logo ni bordure (évite de tronquer la carte).
fn render_welcome_compact(frame: &mut Frame, area: Rect, info: &[Line<'static>]) {
    let w = info.iter().map(|l| l.width()).max().unwrap_or(1).max(1) as u16;
    let h = (info.len() as u16).max(1);
    let rect = centered_rect(area, w, h);
    frame.render_widget(Paragraph::new(info.to_vec()), rect);
}

/// Menu de complétion slash (façon Grok) : une ligne par item (label aligné +
/// indice faint), la sélection sur fond surligné avec un `›`. Sert tous les
/// sous-menus (commandes, modèles, sessions, providers) : les items inactifs sont
/// grisés, un indice `✓` (connecté) passe en accent, les labels longs sont coupés.
fn render_command_menu(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    matches: &[MenuItem],
) {
    let sel = state.completion_index.min(matches.len().saturating_sub(1));
    let width = area.width as usize;
    let namecol = matches
        .iter()
        .map(|m| m.label.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 48);

    let mut lines: Vec<Line> = Vec::with_capacity(matches.len() + 1);
    for (i, item) in matches.iter().enumerate() {
        let selected = i == sel;
        let marker = if selected { "› " } else { "  " };
        let marker_st = if selected {
            theme.accent()
        } else {
            theme.faint()
        };
        // Inactif → grisé ; actif → fg (gras si sélectionné).
        let name_st = if !item.enabled {
            theme.faint()
        } else if selected {
            theme.fg().add_modifier(Modifier::BOLD)
        } else {
            theme.fg()
        };
        // Badge de statut : ✓ connecté (accent), ✗ échec (erreur), ◯ en cours
        // (dim), autres indices en sourdine.
        let hint_st = if item.hint.starts_with('✓') {
            theme.accent()
        } else if item.hint.starts_with('✗') {
            theme.error()
        } else if item.hint.starts_with('◯') {
            theme.dim()
        } else {
            theme.faint()
        };
        let name_disp = fit(&item.label, namecol);
        let desc_room = width.saturating_sub(2 + namecol + 2).max(1);
        let desc_disp = fit(&item.hint, desc_room);
        let desc_len = desc_disp.chars().count();
        let mut spans = vec![
            Span::styled(marker, marker_st),
            Span::styled(format!("{name_disp:<namecol$}"), name_st),
            Span::raw("  "),
            Span::styled(desc_disp, hint_st),
        ];
        // Remplit la fin de ligne pour étaler le fond surligné sur toute la largeur.
        let used = 2 + namecol + 2 + desc_len;
        if width > used {
            spans.push(Span::raw(" ".repeat(width - used)));
        }
        let line = Line::from(spans);
        lines.push(if selected {
            line.style(theme.selection())
        } else {
            line
        });
    }
    lines.push(Line::from(Span::styled(
        format!("{INDENT}↑↓ naviguer · ⏎ exécuter · ⇥ compléter · esc annuler"),
        theme.faint(),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

/// Tronque `s` à `width` colonnes (ellipse `…` si dépassement).
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    let last = state.blocks.len().saturating_sub(1);
    let mut prev: Option<&Block> = None;
    for (i, block) in state.blocks.iter().enumerate() {
        if leading_blank(prev, block) {
            lines.push(Line::default());
        }
        push_block(&mut lines, block, theme, i == last);
        prev = Some(block);
    }

    // Auto-follow : collé au bas (scroll 0), décalé par le scroll utilisateur. La
    // borne se calcule sur les lignes APRÈS wrap (`line_count`), sinon on ne peut
    // pas remonter tout en haut dès qu'une ligne wrappe. On publie la borne dans
    // `scroll_max` pour que l'entrée clampe le scroll sans recalculer le wrap.
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_off = (para.line_count(area.width) as u16).saturating_sub(area.height);
    state.scroll_max.set(max_off);
    let offset = max_off - state.scroll.min(max_off);

    frame.render_widget(para.scroll((offset, 0)), area);
}

/// Faut-il une ligne vide AVANT ce bloc ? On groupe les outils (call/result
/// consécutifs collés) et on aère le reste.
fn leading_blank(prev: Option<&Block>, cur: &Block) -> bool {
    match cur {
        Block::ToolResult { .. } => false,
        Block::ToolCall { .. } => !matches!(
            prev,
            Some(Block::ToolCall { .. } | Block::ToolResult { .. })
        ),
        _ => prev.is_some(),
    }
}

/// Verbe d'action compact pour l'affichage d'un outil (façon Grok).
fn tool_verb(name: &str) -> String {
    match name {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "glob" => "List",
        "grep" => "Search",
        "bash" => "Run",
        other => other,
    }
    .to_string()
}

fn push_block(lines: &mut Vec<Line>, block: &Block, theme: &Theme, is_last: bool) {
    match block {
        Block::User(text) => {
            lines.push(Line::from(vec![
                Span::styled("› ", theme.dim()),
                Span::styled(text.clone(), theme.fg().add_modifier(Modifier::BOLD)),
            ]));
        }
        Block::Assistant { text, .. } => {
            // Markdown rendu, aligné nu (pas de gouttière par-ligne : elle se
            // briserait sur les continuations wrappées). Wrap propre via ratatui.
            lines.extend(crate::markdown::render_markdown(&sanitize(text), theme));
        }
        Block::Reasoning(text) => {
            // Replié en un libellé discret ; en cours (dernier bloc), un court
            // aperçu des dernières lignes pensées (façon « Thinking… »).
            lines.push(Line::from(vec![
                Span::styled(format!("{INDENT}· "), theme.faint()),
                Span::styled("réflexion", theme.faint().add_modifier(Modifier::ITALIC)),
            ]));
            if is_last {
                for raw in preview_tail(&sanitize(text), 2) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{INDENT}  "), theme.faint()),
                        Span::styled(raw, theme.faint().add_modifier(Modifier::ITALIC)),
                    ]));
                }
            }
        }
        Block::ToolCall { name, summary } => {
            lines.push(Line::from(vec![
                Span::styled(format!("{INDENT}◆ "), theme.faint()),
                Span::styled(tool_verb(name), theme.fg().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(truncate(summary, 80), theme.dim()),
            ]));
        }
        Block::ToolResult {
            content, is_error, ..
        } => {
            // Résultat masqué par défaut (transcript clean) ; seules les erreurs
            // remontent, en une ligne.
            if *is_error {
                let clean = sanitize(content);
                let first = clean.lines().next().unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("{INDENT}✗ "), theme.error()),
                    Span::styled(truncate(first, 120), theme.error()),
                ]));
            }
        }
        Block::Notice(text) => {
            lines.push(Line::from(Span::styled(
                format!("{INDENT}· {text}"),
                theme.dim(),
            )));
        }
        Block::Error(text) => {
            lines.push(Line::from(vec![
                Span::styled(format!("{INDENT}✗ "), theme.error()),
                Span::styled(text.clone(), theme.error()),
            ]));
        }
    }
}

/// Nettoie un texte modèle : retire CR, séquences ANSI (CSI) et contrôles C0 —
/// les résidus qui « fuyaient » à droite — et convertit les tabs en espaces.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // séquence `ESC [ … <final>` → on saute jusqu'à l'octet final.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Dernières `n` lignes non vides, markdown allégé et tronquées — pour l'aperçu
/// du raisonnement en cours.
fn preview_tail(text: &str, n: usize) -> Vec<String> {
    let kept: Vec<String> = text
        .lines()
        .map(strip_md)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let start = kept.len().saturating_sub(n);
    kept[start..].iter().map(|l| truncate(l, 100)).collect()
}

fn strip_md(line: &str) -> String {
    line.replace(['*', '`'], "")
        .trim_start_matches('#')
        .trim_start()
        .to_string()
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).split(area);

    // Box de saisie : filets haut/bas SEULEMENT — pas de bordure latérale. Un `│`
    // à gauche/droite polluerait la sélection lors d'un copier-coller depuis le
    // terminal (comme Claude Code). Le filet est discret au repos et s'allume
    // (accent) tant que l'agent réfléchit — même grammaire que la gouttière `▌`.
    // Aucun padding : le `›` est collé au bord gauche, aligné avec les filets.
    let border = match state.status {
        Status::Thinking => theme.accent(),
        Status::Idle => theme.dim(),
    };
    let boundary = Boundary::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(border);
    let inner = boundary.inner(rows[0]);
    frame.render_widget(boundary, rows[0]);

    // Saisie : prompt accent + texte (skills surlignés). Le curseur est le VRAI
    // curseur terminal, positionné plus bas — pas de glyphe dans le buffer.
    let mut spans = vec![Span::styled("› ", theme.accent())];
    spans.extend(input_spans(&state.input, &state.skills, theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    // Curseur réel à la position char `state.cursor` (largeur ≈ nombre de chars).
    let col = inner
        .x
        .saturating_add(2) // largeur du prompt `› `
        .saturating_add(state.cursor as u16)
        .min(inner.right().saturating_sub(1));
    frame.set_cursor_position((col, inner.y));

    render_status_line(frame, rows[1], state, theme);
}

/// Découpe l'input en spans : chaque token `/<skill>` reconnu passe en
/// surbrillance (pastille), le reste en `fg`. Les espaces sont préservés.
fn input_spans(input: &str, skills: &[String], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, part) in input.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", theme.fg()));
        }
        if part.is_empty() {
            continue;
        }
        // Surbrillance : un `/skill` reconnu (n'importe où) OU une commande Numen
        // en 1er token (ex `/goal`, `/models`).
        let is_skill = part
            .strip_prefix('/')
            .is_some_and(|name| skills.iter().any(|s| s == name));
        let is_command = i == 0 && COMMANDS.iter().any(|(name, _, _)| *name == part);
        let style = if is_skill || is_command {
            theme.skill_chip()
        } else {
            theme.fg()
        };
        spans.push(Span::styled(part.to_string(), style));
    }
    spans
}

/// Status line sous la box : `workspace · modèle · contexte` à gauche, état à
/// droite. Séparateur = point médian faint ; seul l'état peut prendre l'accent
/// (pendant la réflexion).
fn render_status_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut left: Vec<Span> = Vec::new();
    if !state.workspace.is_empty() {
        left.push(Span::styled(state.workspace.clone(), theme.dim()));
        left.push(Span::styled(" · ", theme.faint()));
    }
    // Badge fournisseur : `✓ codex` en accent quand connecté (cf. `/providers`).
    if state.provider_connected {
        left.push(Span::styled("✓ codex", theme.accent()));
        left.push(Span::styled(" · ", theme.faint()));
    }
    left.push(Span::styled(state.model.clone(), theme.dim()));
    if let Some(pct) = state.context_pct {
        left.push(Span::styled(" · ", theme.faint()));
        left.push(Span::styled(context_gauge(pct), theme.faint()));
        left.push(Span::styled(format!(" {pct}% contexte"), theme.dim()));
    }

    let (word, word_style) = match state.status {
        Status::Thinking => ("● réfléchit", theme.accent()),
        Status::Idle => ("○ prêt", theme.faint()),
    };
    // Réserve à droite la largeur du mot d'état + une marge symétrique à l'INDENT.
    let right_w = word.chars().count() as u16 + INDENT.len() as u16;
    let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(right_w)]).split(area);

    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(word, word_style),
            Span::raw(INDENT),
        ]))
        .alignment(Alignment::Right),
        cols[1],
    );
}

/// Jauge de contexte compacte en 8 cellules (`▰` plein / `▱` vide), arrondie.
fn context_gauge(pct: u8) -> String {
    let filled = ((pct as usize * 8 + 50) / 100).min(8);
    (0..8).map(|i| if i < filled { '▰' } else { '▱' }).collect()
}

fn render_permission(frame: &mut Frame, area: Rect, prompt: &PermissionPrompt, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    // Titre : un accent net, sans boîte.
    lines.push(Line::from(vec![
        Span::styled("⟐ ", theme.accent()),
        Span::styled(
            prompt.title.clone(),
            theme.fg().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  — {}", prompt.reason), theme.dim()),
    ]));

    // Détail / diff : gouttière non sélectionnable (numéro de ligne faint).
    for (i, dl) in prompt.detail.iter().enumerate() {
        let (sign, sign_style, text_style) = match dl.kind {
            DiffKind::Add => ("+", theme.accent(), theme.fg()),
            DiffKind::Remove => ("-", theme.dim(), theme.dim()),
            DiffKind::Context => (" ", theme.faint(), theme.dim()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:>3} ", i + 1), theme.faint()), // gouttière n°
            Span::styled(format!("{sign} "), sign_style),
            Span::styled(truncate(&dl.text, 140), text_style),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("  [o]", theme.accent()),
        Span::styled(" autoriser   ", theme.dim()),
        Span::styled("[n]", theme.accent()),
        Span::styled(" refuser", theme.dim()),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Hauteur nécessaire au dialog de permission (titre + détail + actions).
fn permission_height(prompt: &PermissionPrompt, _width: u16) -> u16 {
    let detail = prompt.detail.len().min(12) as u16;
    (2 + detail).clamp(2, 16)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DiffLine;
    use agent_core::AgentEvent;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn dump(buf: &Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn draw(state: &AppState, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, state)).unwrap();
        dump(term.backend().buffer())
    }

    // US-019 AC1 : texte streamé rendu token-par-token (markdown), prompt présent.
    #[test]
    fn streamed_text_renders() {
        let mut s = AppState::new("gpt-5", true);
        for tok in ["Bonjour ", "depuis ", "Numen"] {
            s.apply(&AgentEvent::Text(tok.into()));
        }
        let out = draw(&s, 40, 12);
        assert!(out.contains("Bonjour depuis Numen"), "{out}");
        assert!(out.contains("›"), "prompt de saisie absent");
    }

    // Écran d'accueil : carte avec logo braille (Dyson) + identité, transcript vide.
    #[test]
    fn welcome_card_shows_logo_and_brand() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "numen".into();
        s.provider_connected = true;
        assert!(s.is_welcome(), "transcript vide → accueil");
        let out = draw(&s, 80, 24);
        assert!(out.contains("NUMEN"), "marque absente:\n{out}");
        // Le logo est en points braille (U+2801..=U+28FF, hors blanc U+2800).
        assert!(
            out.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
            "logo braille absent:\n{out}"
        );
        assert!(out.contains("/help"), "raccourcis absents:\n{out}");
        assert!(out.contains("gpt-5.5"), "modèle absent:\n{out}");
    }

    // L'accueil disparaît dès le premier message (transcript non vide).
    #[test]
    fn welcome_disappears_after_first_message() {
        let mut s = AppState::new("gpt-5.5", true);
        s.push_user("salut");
        assert!(!s.is_welcome());
        let out = draw(&s, 80, 24);
        assert!(out.contains("salut"));
        assert!(!out.contains("NUMEN"), "accueil doit s'effacer:\n{out}");
    }

    // Terminal trop étroit pour la carte → repli compact, sans panic, marque visible.
    #[test]
    fn welcome_falls_back_compact_on_small_terminal() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "numen".into();
        let out = draw(&s, 30, 8);
        assert!(
            out.contains("NUMEN"),
            "repli compact doit garder la marque:\n{out}"
        );
    }

    // Le markdown est rendu, pas affiché en brut (les `**` disparaissent).
    #[test]
    fn markdown_bold_is_not_shown_raw() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("Voici **important** ici".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 50, 10);
        assert!(out.contains("important"), "{out}");
        assert!(!out.contains("**"), "markdown brut non rendu:\n{out}");
    }

    // US-019 AC2 : un diff avec gouttière (numéros) s'affiche dans le dialog.
    #[test]
    fn permission_dialog_renders_diff_gutter() {
        let mut s = AppState::new("gpt-5", true);
        s.pending = Some(PermissionPrompt {
            title: "edit src/main.rs".into(),
            reason: "mutation".into(),
            detail: vec![
                DiffLine {
                    kind: DiffKind::Remove,
                    text: "let x = 1;".into(),
                },
                DiffLine {
                    kind: DiffKind::Add,
                    text: "let x = 2;".into(),
                },
            ],
        });
        let out = draw(&s, 50, 14);
        assert!(
            out.contains("autoriser") && out.contains("refuser"),
            "{out}"
        );
        assert!(
            out.contains("- let x = 1;"),
            "ligne supprimée absente:\n{out}"
        );
        assert!(
            out.contains("+ let x = 2;"),
            "ligne ajoutée absente:\n{out}"
        );
        assert!(out.contains("edit src/main.rs"));
    }

    // US-019 AC4 : dégradation sans truecolor — pas de panic, layout intact.
    #[test]
    fn monochrome_degradation_renders_without_panic() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("texte mono".into()));
        let out = draw(&s, 30, 8);
        assert!(out.contains("texte mono"));
    }

    // US-019 AC4 (bis) : terminal étroit → reflow sans corruption (pas de panic).
    #[test]
    fn narrow_terminal_does_not_corrupt() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text(
            "un texte assez long pour devoir wrapper sur plusieurs lignes dans un terminal étroit"
                .into(),
        ));
        let _ = draw(&s, 16, 10);
        let _ = draw(&s, 8, 6);
        // pas de panic = indices de wrap recalculés proprement.
    }

    // Scroll : la borne est calculée sur les lignes APRÈS wrap, donc on peut
    // remonter jusqu'au tout premier tour même quand le contenu wrappe.
    #[test]
    fn scroll_up_reaches_top_of_wrapped_transcript() {
        let mut s = AppState::new("gpt-5", true);
        for i in 0..10 {
            s.push_user(format!("message numéro {i} avec un peu de texte en plus"));
            s.apply(&AgentEvent::Text(format!("réponse {i}")));
            s.apply(&AgentEvent::EndTurn);
        }
        // 1er rendu : publie scroll_max (le transcript déborde la fenêtre étroite).
        let _ = draw(&s, 24, 8);
        assert!(
            s.scroll_max.get() > 0,
            "transcript débordant → scroll_max > 0"
        );
        // remonter au-delà de la borne est clampé ; le 1er tour devient visible.
        s.scroll_up(1000);
        assert_eq!(s.scroll, s.scroll_max.get(), "scroll clampé à la borne");
        let out = draw(&s, 24, 8);
        assert!(
            out.contains("message numéro 0"),
            "le haut du transcript doit être atteignable:\n{out}"
        );
    }

    // Refus de permission interrompt proprement (état nettoyé) — AC3.
    #[test]
    fn refusing_permission_clears_prompt() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = AppState::new("gpt-5", true);
        s.pending = Some(PermissionPrompt {
            title: "bash".into(),
            reason: "sensible".into(),
            detail: vec![],
        });
        let action = s.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, crate::state::InputAction::Permission(false));
        assert!(s.pending.is_none());
    }
}
