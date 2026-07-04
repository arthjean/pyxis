//! Rendu Ratatui (US-019). Esthétique : **monochrome + un accent**, épurée,
//! aucune bordure lourde. Hiérarchie par poids/teinte et espace négatif, pas par
//! couleur. Signature visuelle : une gouttière `▌` qui s'allume (accent) sur le
//! tour assistant en cours de stream, et se calme (faint) une fois fini.
//!
//! `render` est PUR → testable via `TestBackend`. La dégradation sans truecolor
//! (AC4) remplace l'accent par du gras ; la mise en page est inchangée.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as Boundary, BorderType, Borders, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;

use agent_core::ToolErrorKind;

use crate::cache::fingerprint;
use crate::measure;
use crate::state::{AppState, Block, COMMANDS, MenuItem, PermissionPrompt, Status};
use crate::theme::Theme;
use crate::tool;

const INDENT: &str = "  ";
/// Zone de saisie : box bordée (3 lignes) + ligne de statut (1).
const INPUT_HEIGHT: u16 = 4;
const PROGRESS_HEIGHT: u16 = 1;
const PROGRESS_GAP_HEIGHT: u16 = 1;
const MENU_MAX_ITEMS: u16 = 8;

/// Rendu complet d'une frame.
pub fn render(frame: &mut Frame, state: &AppState) {
    let theme = Theme::new(state.truecolor);
    let area = frame.area();

    // En bas : soit le dialog de permission, soit (status + input).
    let bottom_height = match &state.pending {
        Some(p) => permission_height(p, area.width),
        None => input_height(state),
    };
    // Menu de commandes slash : popup intercalé entre transcript et input (jamais
    // pendant un dialog de permission). +1 ligne pour le rappel des raccourcis.
    let matches = state.menu_items();
    let menu_open = state.pending.is_none() && !state.shutdown_in_progress() && !matches.is_empty();
    let max_menu_height = area.height.saturating_sub(bottom_height).saturating_sub(1);
    let menu_height = if menu_open {
        ((matches.len() as u16).min(MENU_MAX_ITEMS) + 1).min(max_menu_height)
    } else {
        0
    };
    let menu = menu_open && menu_height > 0;

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

#[cfg(feature = "codex_tui_parity")]
pub fn render_parity(
    frame: &mut Frame,
    state: &AppState,
    surface: &crate::history_cell::ChatSurface,
) {
    let theme = Theme::new(state.truecolor);
    let area = frame.area();

    let bottom_height = match &state.pending {
        Some(p) => permission_height(p, area.width),
        None => input_height(state),
    };
    let matches = state.menu_items();
    let menu_open = state.pending.is_none() && !state.shutdown_in_progress() && !matches.is_empty();
    let max_menu_height = area.height.saturating_sub(bottom_height).saturating_sub(1);
    let menu_height = if menu_open {
        ((matches.len() as u16).min(MENU_MAX_ITEMS) + 1).min(max_menu_height)
    } else {
        0
    };
    let menu = menu_open && menu_height > 0;

    if state.is_welcome()
        && surface.transcript_cells().is_empty()
        && surface.active_cell().is_none()
    {
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(menu_height),
            Constraint::Length(bottom_height),
        ])
        .split(area);
        render_welcome(frame, chunks[0], state, &theme);
        if menu {
            render_command_menu(frame, chunks[1], state, &theme, &matches);
        }
        match &state.pending {
            Some(prompt) => render_permission(frame, chunks[2], prompt, &theme),
            None => render_input(frame, chunks[2], state, &theme),
        }
        return;
    }

    let separator_height = u16::from(state.scroll == 0);
    let available_transcript_height = area
        .height
        .saturating_sub(menu_height)
        .saturating_sub(bottom_height)
        .saturating_sub(separator_height);
    let transcript_height = if state.scroll > 0 {
        available_transcript_height
    } else {
        let visible_height = surface
            .display_lines(area.width)
            .len()
            .min(u16::MAX as usize) as u16;
        visible_height.min(available_transcript_height)
    };
    let trailing_height = area
        .height
        .saturating_sub(transcript_height)
        .saturating_sub(separator_height)
        .saturating_sub(menu_height)
        .saturating_sub(bottom_height);
    let chunks = Layout::vertical([
        Constraint::Length(trailing_height),
        Constraint::Length(transcript_height),
        Constraint::Length(separator_height),
        Constraint::Length(menu_height),
        Constraint::Length(bottom_height),
    ])
    .split(area);

    render_parity_transcript(frame, chunks[1], state, surface, &theme);
    if menu {
        render_command_menu(frame, chunks[3], state, &theme, &matches);
    }
    match &state.pending {
        Some(prompt) => render_permission(frame, chunks[4], prompt, &theme),
        None => render_input(frame, chunks[4], state, &theme),
    }
}

#[cfg(feature = "codex_tui_parity")]
fn render_parity_transcript(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    surface: &crate::history_cell::ChatSurface,
    theme: &Theme,
) {
    let all_lines = surface.display_lines(area.width);
    let max_off = all_lines.len().saturating_sub(area.height as usize);
    state.scroll_max.set(max_off);

    let lines = if area.height == 0 {
        Vec::new()
    } else if state.scroll == 0 {
        all_lines
            .into_iter()
            .skip(max_off)
            .take(area.height as usize)
            .collect()
    } else {
        let offset = max_off.saturating_sub(state.scroll.min(max_off));
        all_lines
            .into_iter()
            .skip(offset)
            .take(area.height as usize)
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    render_scroll_pill(frame, area, state, theme);
}

/// Logo de Pyxis : une **sphère de Dyson** minimaliste. La boussole donne le cap
/// dans un espace immense ; ici, un cœur stellaire net cerné de deux anneaux de
/// collecteurs (avec brèches, l'essaim en assemblage). Rendu en **points braille
/// tramés** (stippling, façon
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
            let style = if theme.truecolor() {
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

fn top_centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + u16::from(area.height > h),
        width: w,
        height: h,
    }
}

/// Écran d'accueil (hero façon Grok) : une carte en haut, logo braille (sphère
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
            "PYXIS",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "your terminal coding agent",
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
        meta.push(Span::styled(state.workspace.clone(), theme.dim()));
    }
    meta.push(Span::styled("  ·  ", theme.faint()));
    meta.push(Span::styled(state.permission_mode_label(), theme.dim()));
    info.push(Line::from(meta));
    if state.provider_connected {
        info.push(Line::from(vec![
            Span::styled("✓ codex", theme.accent()),
            Span::styled("  ChatGPT subscription", theme.dim()),
        ]));
    } else {
        info.push(Line::from(vec![
            Span::styled("○ not connected", theme.accent()),
            Span::styled("  restart pyxis to reconnect", theme.dim()),
        ]));
    }
    info.push(Line::default());
    info.push(Line::from(vec![
        Span::styled("/help", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/models", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/permissions", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/goal", theme.accent()),
    ]));
    info.push(Line::from(Span::styled(
        format!("{}   ·   ↑ history", shortcut_hint(state)),
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

    let rect = top_centered_rect(area, card_w, card_h);
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

/// Repli de l'accueil pour terminal étroit : le bloc d'identité seul en haut,
/// sans logo ni bordure (évite de tronquer la carte).
fn render_welcome_compact(frame: &mut Frame, area: Rect, info: &[Line<'static>]) {
    let w = info.iter().map(|l| l.width()).max().unwrap_or(1).max(1) as u16;
    let h = (info.len() as u16).max(1);
    let rect = top_centered_rect(area, w, h);
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
    if area.height == 0 {
        return;
    }
    let sel = state.completion_index.min(matches.len().saturating_sub(1));
    let width = area.width as usize;
    let visible_items = (area.height as usize).saturating_sub(1).min(matches.len());
    if visible_items == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{INDENT}↑↓ navigate · enter run · tab complete · esc cancel"),
                theme.faint(),
            ))),
            area,
        );
        return;
    }
    let start = sel.saturating_add(1).saturating_sub(visible_items);
    let end = (start + visible_items).min(matches.len());
    let namecol = matches
        .iter()
        .map(|m| measure::width(&m.label))
        .max()
        .unwrap_or(8)
        .clamp(8, 48);

    let mut lines: Vec<Line> = Vec::with_capacity(visible_items + 1);
    for (offset, item) in matches[start..end].iter().enumerate() {
        let i = start + offset;
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
        let desc_len = measure::width(&desc_disp);
        let mut spans = vec![
            Span::styled(marker, marker_st),
            Span::styled(measure::pad_right(name_disp, namecol), name_st),
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
    let footer = if matches.len() > visible_items {
        format!(
            "{INDENT}{}-{}/{} · ↑↓ navigate · enter run · tab complete · esc cancel",
            start + 1,
            end,
            matches.len()
        )
    } else {
        format!("{INDENT}↑↓ navigate · enter run · tab complete · esc cancel")
    };
    lines.push(Line::from(Span::styled(footer, theme.faint())));

    frame.render_widget(Paragraph::new(lines), area);
}

/// Tronque `s` à `width` colonnes (ellipse `…` si dépassement).
fn fit(s: &str, width: usize) -> String {
    measure::truncate(s, width)
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let width = area.width as usize;
    // Index des appels d'outils par id : apparie un ToolResult à son ToolCall
    // (US-033) pour dériver le résumé `⎿` depuis l'input du call.
    let mut calls: std::collections::HashMap<&str, (&str, &serde_json::Value, u64)> =
        std::collections::HashMap::new();
    for block in &state.blocks {
        if let Block::ToolCall {
            id,
            name,
            input,
            input_hash,
        } = block
        {
            calls.insert(id.as_str(), (name.as_str(), input, *input_hash));
        }
    }

    // Cache des lignes stylées par bloc (US-041) : on ne reconstruit (parse markdown
    // + coloration) que les blocs dont l'empreinte a changé — typiquement le seul
    // bloc en cours de stream. Les autres sont servis depuis le cache. `render` reste
    // pur : le cache vit en interior mutability sur `AppState`.
    let last = state.blocks.len().saturating_sub(1);
    let mut cache = state.render_cache.borrow_mut();
    cache.begin(width, state.truecolor, state.blocks.len());

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Block> = None;
    for (i, block) in state.blocks.iter().enumerate() {
        let is_last = i == last;
        let fp = fingerprint(block, is_last, &calls);
        let blk = cache.block_lines(i, fp, || {
            let mut v = Vec::new();
            push_block(&mut v, block, theme, is_last, width, &calls);
            v
        });
        // Un tour assistant vide (avant le 1er token, ou texte purement blanc) rend
        // zéro ligne → pas de puce orpheline (US-034) et `prev` reste inchangé.
        if blk.is_empty() {
            continue;
        }
        if leading_blank(prev, block) {
            lines.push(Line::default());
        }
        lines.extend(blk.iter().cloned());
        prev = Some(block);
    }
    drop(cache);

    // Le wrap manuel ci-dessus pose la gouttière suspendue (puce + indent 2 col) pour
    // le cas courant (largeur comptée en `char`). On garde `Wrap` comme FILET : une
    // ligne qui dépasserait la largeur en COLONNES (wide chars CJK/emoji, que le
    // compte en `char` ne voit pas) est re-wrappée par ratatui plutôt que TRONQUÉE
    // (aucune perte). La borne de scroll se calcule donc sur les lignes APRÈS wrap.
    let max_off = lines.len().saturating_sub(area.height as usize);
    state.scroll_max.set(max_off);
    let offset = max_off.saturating_sub(state.scroll.min(max_off));
    let visible = lines
        .into_iter()
        .skip(offset)
        .take(area.height as usize)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);
    render_scroll_pill(frame, area, state, theme);
}

/// Pill discrète « nouveaux messages » (US-046) : en bas du transcript quand
/// l'utilisateur a remonté le fil ET que du contenu est arrivé en dessous.
/// Right-alignée, bornée à la largeur (ne déborde pas, ne masque pas l'input qui
/// vit dans une zone séparée). `⇟` = raccourci pour redescendre.
fn render_scroll_pill(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.scroll == 0 || state.unseen == 0 || area.height == 0 {
        return;
    }
    let noun = if state.unseen > 1 { "items" } else { "item" };
    let label = format!(" ↓ {} new {noun} · ⇟ ", state.unseen);
    let w = (measure::width(&label) as u16).min(area.width);
    let pill = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y + area.height - 1,
        width: w,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, theme.accent()))).style(theme.selection()),
        pill,
    );
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

fn push_block<'a>(
    lines: &mut Vec<Line<'static>>,
    block: &'a Block,
    theme: &Theme,
    is_last: bool,
    width: usize,
    calls: &std::collections::HashMap<&'a str, (&'a str, &'a serde_json::Value, u64)>,
) {
    match block {
        Block::User(text) => {
            push_wrapped(
                lines,
                vec![Span::styled(
                    sanitize(text),
                    theme.fg().add_modifier(Modifier::BOLD),
                )],
                Span::styled("› ", theme.dim()),
                Span::raw(INDENT),
                width,
            );
        }
        Block::Assistant { text, streaming } => {
            // Markdown rendu, ANCRÉ par une puce teal ; corps aligné à 2 colonnes
            // (gouttière suspendue : puce sur la 1re sous-ligne, reste indenté). La
            // largeur de CONTENU (hors gouttière) sert à dimensionner les tables
            // markdown (US-043) — même valeur que le wrap d'`emit_block`.
            let avail = width.saturating_sub(INDENT.len());
            let clean = sanitize(text);
            let md = if *streaming {
                crate::markdown::render_markdown_with_highlight(&clean, theme, avail, false)
            } else {
                crate::markdown::render_markdown(&clean, theme, avail)
            };
            emit_block(lines, &md, Span::styled("● ", theme.accent()), width);
        }
        Block::Reasoning(text) => {
            // Replié en un libellé discret ; en cours (dernier bloc), un court
            // aperçu des dernières lignes pensées (façon « Thinking… »).
            lines.push(Line::from(vec![
                Span::styled(format!("{INDENT}· "), theme.faint()),
                Span::styled("thinking", theme.faint().add_modifier(Modifier::ITALIC)),
            ]));
            if is_last {
                let preview_st = theme.faint().add_modifier(Modifier::ITALIC);
                let cont = Span::styled(format!("{INDENT}  "), theme.faint());
                for raw in preview_tail(&sanitize(text), 2) {
                    // Via `push_wrapped` (comme tout autre bloc) : la gouttière
                    // suspendue à 4 colonnes survit au wrap sur terminal étroit
                    // (sinon la 2e sous-ligne revient en colonne 0, US-034).
                    push_wrapped(
                        lines,
                        vec![Span::styled(raw, preview_st)],
                        cont.clone(),
                        cont.clone(),
                        width,
                    );
                }
            }
        }
        Block::ToolCall { name, input, .. } => {
            // Puce grise + label structuré `Verb(cible)` (US-035).
            let label = tool::label(name, input);
            let mut content = vec![Span::styled(
                label.verb,
                theme.fg().add_modifier(Modifier::BOLD),
            )];
            if let Some(t) = label.target {
                content.push(Span::styled(format!("({t})"), theme.dim()));
            }
            push_wrapped(
                lines,
                content,
                Span::styled("● ", theme.faint()),
                Span::raw(INDENT),
                width,
            );
        }
        Block::ToolResult {
            call_id,
            content,
            is_error,
            error_kind,
            ..
        } => {
            if *is_error {
                if matches!(error_kind, Some(ToolErrorKind::PermissionDenied))
                    || tool::is_user_rejection(content)
                {
                    // Rejet volontaire (permission refusée) : ton atténué, pas
                    // rouge : ce n'est pas une erreur système (US-036).
                    push_wrapped(
                        lines,
                        vec![Span::styled(tool::reject_summary(content), theme.dim())],
                        Span::styled(format!("{INDENT}⎿ "), theme.dim()),
                        Span::styled(format!("{INDENT}  "), theme.dim()),
                        width,
                    );
                } else {
                    // Erreur d'outil : connecteur + message rouge, borné à 1 ligne
                    // + indicateur du reste (US-036).
                    push_wrapped(
                        lines,
                        vec![Span::styled(tool::error_summary(content), theme.error())],
                        Span::styled(format!("{INDENT}⎿ "), theme.error()),
                        Span::styled(format!("{INDENT}  "), theme.error()),
                        width,
                    );
                    let extra = tool::extra_lines(content);
                    if extra > 0 {
                        push_wrapped(
                            lines,
                            vec![Span::styled(format!("... +{extra} lines"), theme.faint())],
                            Span::styled(format!("{INDENT}  "), theme.faint()),
                            Span::styled(format!("{INDENT}  "), theme.faint()),
                            width,
                        );
                    }
                }
            } else {
                let call = calls
                    .get(call_id.as_str())
                    .map(|(name, input, _)| (*name, *input));
                // Résumé secondaire `⎿` (nombres mis en évidence) apparié au call.
                push_wrapped(
                    lines,
                    tool::result_summary(call, content, theme),
                    Span::styled(format!("{INDENT}⎿ "), theme.faint()),
                    Span::styled(format!("{INDENT}  "), theme.faint()),
                    width,
                );
                // Diff inline (US-038) : edit/write réussi → diff dérivé de l'input
                // du call (rien pour les lectures ni les outils non mutants).
                if let Some((name, input)) = call
                    && let Some(d) = crate::diff::from_tool(name, input)
                {
                    // Coloration syntaxique du diff (US-042) : langage déduit de
                    // l'extension du chemin édité.
                    let lang = input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .and_then(crate::highlight::lang_from_path);
                    push_diff(lines, &d, theme, width, lang.as_deref());
                }
            }
        }
        Block::Notice(text) => {
            push_wrapped(
                lines,
                vec![Span::styled(text.clone(), theme.dim())],
                Span::styled(format!("{INDENT}· "), theme.dim()),
                Span::styled(format!("{INDENT}  "), theme.dim()),
                width,
            );
        }
        Block::Error(text) => {
            push_wrapped(
                lines,
                vec![Span::styled(text.clone(), theme.error())],
                Span::styled(format!("{INDENT}✗ "), theme.error()),
                Span::styled(format!("{INDENT}  "), theme.error()),
                width,
            );
        }
    }
}

/// Émet un bloc markdown (plusieurs lignes logiques) ancré par `bullet` sur la
/// toute première sous-ligne ; les autres sont indentées à 2 colonnes (gouttière
/// suspendue qui survit au wrap). Bloc vide → rien (pas de puce orpheline, US-034).
fn emit_block(
    lines: &mut Vec<Line<'static>>,
    md: &[Line<'static>],
    bullet: Span<'static>,
    width: usize,
) {
    let cont = Span::raw(INDENT);
    let avail = width.saturating_sub(INDENT.len()).max(1);
    let mut first = true;
    for logical in md {
        for sub in wrap_content(&logical.spans, avail) {
            let lead = if first { bullet.clone() } else { cont.clone() };
            let mut spans = vec![lead];
            spans.extend(sub);
            lines.push(Line::from(spans));
            first = false;
        }
    }
}

/// Pousse une ligne logique `content` wrappée à `width`, `first` en tête de la 1re
/// sous-ligne et `cont` des suivantes (préfixes de même largeur → alignement
/// propre). Préserve les styles ; coupe les mots trop longs.
fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    content: Vec<Span<'static>>,
    first: Span<'static>,
    cont: Span<'static>,
    width: usize,
) {
    let prefix_w =
        measure::width(first.content.as_ref()).max(measure::width(cont.content.as_ref()));
    let avail = width.saturating_sub(prefix_w).max(1);
    for (i, sub) in wrap_content(&content, avail).into_iter().enumerate() {
        let lead = if i == 0 { first.clone() } else { cont.clone() };
        let mut spans = vec![lead];
        spans.extend(sub);
        lines.push(Line::from(spans));
    }
}

/// Word-wrap d'une suite de spans à `width` colonnes terminal, styles préservés.
/// Coupe au dernier espace ; à défaut (mot plus long que `width`), coupe dur.
/// Retourne au moins une sous-ligne (éventuellement vide).
fn wrap_content(spans: &[Span], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut units: Vec<(String, Style, usize)> = Vec::new();
    for s in spans {
        for g in s.content.as_ref().graphemes(true) {
            units.push((g.to_string(), s.style, measure::width(g)));
        }
    }
    if width == 0 || units.is_empty() {
        return vec![rebuild(&units)];
    }
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<(String, Style, usize)> = Vec::new();
    let mut line_w = 0usize;
    let mut last_space: Option<usize> = None;
    for (g, st, gw) in units {
        line_w += gw;
        line.push((g, st, gw));
        if line.last().is_some_and(|(g, _, _)| g == " ") {
            last_space = Some(line.len() - 1);
        }
        if line_w > width {
            if let Some(sp) = last_space {
                let rest = line.split_off(sp + 1);
                line.pop(); // retire l'espace de coupure
                out.push(rebuild(&line));
                line = rest;
                line_w = line.iter().map(|(_, _, w)| *w).sum();
            } else {
                let overflow = line.pop();
                out.push(rebuild(&line));
                line.clear();
                if let Some(lc) = overflow {
                    line_w = lc.2;
                    line.push(lc);
                } else {
                    line_w = 0;
                }
            }
            last_space = None;
        }
    }
    out.push(rebuild(&line));
    out
}

/// Recompose une suite `(grapheme, style)` en spans, en fusionnant les runs de même style.
fn rebuild(units: &[(String, Style, usize)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (g, st, _) in units {
        if cur != Some(*st) {
            if let Some(prev) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            cur = Some(*st);
        }
        buf.push_str(g);
    }
    if let Some(prev) = cur {
        spans.push(Span::styled(buf, prev));
    }
    spans
}

/// Nettoie un texte modèle : retire CR, séquences ANSI (CSI) et contrôles C0 —
/// les résidus qui « fuyaient » à droite — et convertit les tabs en espaces.
/// Rend un diff structuré (US-038) sous le résumé `⎿` : gouttière de numéros
/// (relatifs), signe `+`/`-`, fonds vert/rouge en truecolor (ou signe + gras/dim en
/// 16 couleurs), emphase mot-à-mot saturée. Les lignes trop larges sont tronquées
/// sans corrompre la gouttière (qui reste en tête de ligne).
fn push_diff(
    lines: &mut Vec<Line<'static>>,
    diff: &crate::diff::Diff,
    theme: &Theme,
    width: usize,
    lang: Option<&str>,
) {
    use crate::diff::Row;
    let gw = diff
        .rows
        .iter()
        .filter_map(Row::lineno)
        .max()
        .unwrap_or(0)
        .to_string()
        .chars()
        .count()
        .max(2);
    for row in &diff.rows {
        match row {
            Row::Add { lineno, segs } => {
                let colors = line_colors_for(segs, lang, theme);
                let mut spans = vec![
                    gutter(*lineno, gw, theme),
                    Span::styled("+ ", theme.diff_add()),
                ];
                spans.extend(diff_segs_spans(
                    segs,
                    colors.as_deref(),
                    theme.diff_add(),
                    theme.diff_add_word(),
                ));
                lines.push(fill(spans, theme.diff_add(), width));
            }
            Row::Remove { lineno, segs } => {
                let colors = line_colors_for(segs, lang, theme);
                let mut spans = vec![
                    gutter(*lineno, gw, theme),
                    Span::styled("- ", theme.diff_remove()),
                ];
                spans.extend(diff_segs_spans(
                    segs,
                    colors.as_deref(),
                    theme.diff_remove(),
                    theme.diff_remove_word(),
                ));
                lines.push(fill(spans, theme.diff_remove(), width));
            }
            Row::Context { lineno, text } => {
                let colors = lang.and_then(|l| crate::highlight::line_colors(text, l, theme));
                let seg = [crate::diff::Seg {
                    text: text.clone(),
                    emphasized: false,
                }];
                let mut spans = vec![
                    gutter(*lineno, gw, theme),
                    Span::styled("  ", theme.faint()),
                ];
                spans.extend(diff_segs_spans(
                    &seg,
                    colors.as_deref(),
                    theme.dim(),
                    theme.dim(),
                ));
                lines.push(clip(spans, width));
            }
            Row::Gap => {
                let pad = " ".repeat(gw);
                lines.push(Line::from(Span::styled(
                    format!("{INDENT}{pad} ⋮"),
                    theme.faint(),
                )));
            }
            Row::Truncated(n) => {
                lines.push(Line::from(Span::styled(
                    format!("{INDENT}… +{n} lines"),
                    theme.faint(),
                )));
            }
        }
    }
}

/// Couleurs de syntaxe (une par caractère) d'une ligne de diff reconstruite depuis
/// ses segments. `None` si pas de langage, pas de truecolor, ou langage non couvert.
fn line_colors_for(
    segs: &[crate::diff::Seg],
    lang: Option<&str>,
    theme: &Theme,
) -> Option<Vec<Color>> {
    let lang = lang?;
    let line: String = segs.iter().map(|s| s.text.as_str()).collect();
    crate::highlight::line_colors(&line, lang, theme)
}

/// Spans colorés du contenu d'une ligne de diff (US-042). Les segments emphasés
/// (word-diff) gardent leur style saturé `word` ; les autres reçoivent la teinte de
/// syntaxe `colors[ci]` sur le fond `base` (le signe `+`/`-` et le fond ajout/
/// suppression, posés par l'appelant, ne sont jamais masqués). `colors = None` →
/// tout en `base` (rendu historique). Les runs de même style sont fusionnés.
fn diff_segs_spans(
    segs: &[crate::diff::Seg],
    colors: Option<&[Color]>,
    base: Style,
    word: Style,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    let mut ci = 0usize;
    for seg in segs {
        for ch in seg.text.chars() {
            let style = if seg.emphasized {
                word
            } else {
                match colors.and_then(|c| c.get(ci)) {
                    Some(col) => base.fg(*col),
                    None => base,
                }
            };
            if cur != Some(style) {
                if let Some(prev) = cur.take() {
                    out.push(Span::styled(std::mem::take(&mut buf), prev));
                }
                cur = Some(style);
            }
            buf.push(ch);
            ci += 1;
        }
    }
    if let Some(prev) = cur {
        out.push(Span::styled(buf, prev));
    }
    out
}

/// Gouttière de numéro de ligne (faint), `lineno` aligné à droite sur `gw`,
/// précédée de l'indentation du bloc. `None` → colonne vide.
fn gutter(lineno: Option<usize>, gw: usize, theme: &Theme) -> Span<'static> {
    let n = lineno.map(|n| n.to_string()).unwrap_or_default();
    Span::styled(format!("{INDENT}{n:>gw$} "), theme.faint())
}

/// Compose une ligne de diff colorée : si elle dépasse `width`, tronque (gouttière
/// en tête, donc préservée) ; sinon remplit la fin avec `bg` (bande de couleur en
/// truecolor ; sans effet visible en 16 couleurs).
fn fill(spans: Vec<Span<'static>>, bg: Style, width: usize) -> Line<'static> {
    let total: usize = spans
        .iter()
        .map(|s| measure::width(s.content.as_ref()))
        .sum();
    if total >= width {
        let first = wrap_content(&spans, width)
            .into_iter()
            .next()
            .unwrap_or_default();
        Line::from(first)
    } else {
        let mut spans = spans;
        spans.push(Span::styled(" ".repeat(width - total), bg));
        Line::from(spans)
    }
}

/// Tronque une ligne (sans fond) à `width` colonnes, gouttière conservée.
fn clip(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let total: usize = spans
        .iter()
        .map(|s| measure::width(s.content.as_ref()))
        .sum();
    if total > width {
        let first = wrap_content(&spans, width)
            .into_iter()
            .next()
            .unwrap_or_default();
        Line::from(first)
    } else {
        Line::from(spans)
    }
}

pub(crate) fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Toutes les familles d'échappement, pas seulement CSI : une sortie
            // d'outil/modèle adverse peut porter de l'OSC (titre de fenêtre,
            // hyperlink OSC 8, clipboard OSC 52) ou du DCS, qui ré-arment le
            // terminal si on ne neutralise que `ESC [`.
            '\x1b' => match chars.peek().copied() {
                // CSI `ESC [ … <final 0x40..=0x7E>`.
                Some('[') => {
                    chars.next();
                    drain_csi(&mut chars);
                }
                // OSC `ESC ] … <BEL | ST>`, et DCS/SOS/PM/APC `ESC P|X|^|_ … ST`.
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    drain_to_st(&mut chars);
                }
                // ESC à 2 octets (`ESC c` reset, `ESC ( B`…) ou ESC isolé : on jette
                // l'ESC et l'octet intermédiaire éventuel (jamais de séquence émise).
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // Introducteurs C1 8 bits : neutralisés AVEC leur corps (sinon les
            // paramètres « 31m » fuient en texte). CSI=0x9B, OSC=0x9D, DCS/PM/APC.
            '\u{9b}' => drain_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{9e}' | '\u{9f}' => drain_to_st(&mut chars),
            '\r' => {}
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            // C0 (hors \n,\t,\r), DEL (0x7F) et C1 8-bit isolés restants : retirés.
            c if (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Draine une séquence CSI jusqu'à son octet final (`0x40..=0x7E`), terminateur inclus.
fn drain_csi<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    for n in chars.by_ref() {
        if ('@'..='~').contains(&n) {
            break;
        }
    }
}

/// Draine jusqu'au String Terminator (`ESC \`) ou BEL (`\x07`) — fin d'une séquence
/// OSC/DCS. Consomme le terminateur ; s'arrête aussi sur un ESC nu (séquence malformée).
fn drain_to_st<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    while let Some(n) = chars.next() {
        if n == '\u{07}' {
            break;
        }
        if n == '\x1b' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            break;
        }
    }
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
    let footer_height = u16::from(!state.shutdown_in_progress());
    let (progress_area, composer_area, footer_area) = if progress_visible(state) {
        let rows = Layout::vertical([
            Constraint::Length(PROGRESS_HEIGHT),
            Constraint::Length(PROGRESS_GAP_HEIGHT),
            Constraint::Length(3),
            Constraint::Length(footer_height),
        ])
        .split(area);
        (Some(rows[0]), rows[2], rows[3])
    } else {
        let rows = Layout::vertical([Constraint::Length(3), Constraint::Length(footer_height)])
            .split(area);
        (None, rows[0], rows[1])
    };

    if let Some(progress_area) = progress_area {
        render_progress_line(frame, progress_area, state, theme);
    }

    let fill = (0..composer_area.height)
        .map(|_| Line::from(Span::raw(" ".repeat(composer_area.width as usize))))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(fill).style(theme.composer()), composer_area);

    let inner = Rect {
        x: composer_area.x,
        y: composer_area.y + composer_area.height / 2,
        width: composer_area.width,
        height: 1,
    };

    let mut spans = vec![Span::styled("› ", theme.fg().add_modifier(Modifier::BOLD))];
    if state.shutdown_in_progress() {
        spans.push(Span::styled("Shutting down...", theme.dim()));
    } else {
        spans.extend(input_spans(
            &state.input,
            &state.skills,
            &state.files,
            theme,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    if !state.shutdown_in_progress() {
        let cursor_prefix = state.input.get(..state.cursor).unwrap_or(&state.input);
        let col = inner
            .x
            .saturating_add(2)
            .saturating_add(measure::width(cursor_prefix) as u16)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position((col, inner.y));
    }

    if !state.shutdown_in_progress() {
        render_status_line(frame, footer_area, state, theme);
    }
}

fn input_height(state: &AppState) -> u16 {
    if state.shutdown_in_progress() {
        return 3;
    }
    if progress_visible(state) {
        INPUT_HEIGHT + PROGRESS_HEIGHT + PROGRESS_GAP_HEIGHT
    } else {
        INPUT_HEIGHT
    }
}

fn progress_visible(state: &AppState) -> bool {
    matches!(state.status, Status::Thinking)
}

/// Découpe l'input en spans : chaque token `/<skill>` reconnu passe en
/// surbrillance (pastille), le reste en `fg`. Les espaces sont préservés.
fn input_spans(
    input: &str,
    skills: &[String],
    files: &[String],
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, part) in input.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", theme.fg()));
        }
        if part.is_empty() {
            continue;
        }
        // Surbrillance : un `/skill` reconnu (n'importe où) OU une commande Pyxis
        // en 1er token (ex `/goal`, `/models`).
        let is_skill = part
            .strip_prefix('/')
            .is_some_and(|name| skills.iter().any(|s| s == name));
        let is_file = part
            .strip_prefix('@')
            .is_some_and(|path| files.iter().any(|f| f == path));
        let is_command = i == 0 && COMMANDS.iter().any(|(name, _, _)| *name == part);
        let style = if is_skill || is_file || is_command {
            theme.skill_chip()
        } else {
            theme.fg()
        };
        spans.push(Span::styled(part.to_string(), style));
    }
    spans
}

fn render_status_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut left: Vec<Span> = Vec::new();
    left.push(Span::styled(
        state.model.clone(),
        theme.fg().add_modifier(Modifier::BOLD),
    ));
    if let Some(effort) = &state.reasoning_effort
        && !effort.trim().is_empty()
    {
        left.push(Span::styled(format!(" {}", effort.trim()), theme.dim()));
    }
    if !state.workspace.is_empty() {
        left.push(Span::styled(" · ", theme.faint()));
        left.push(Span::styled(state.workspace.clone(), theme.success()));
    }
    left.push(Span::styled(" · ", theme.faint()));
    left.push(Span::styled(state.permission_mode_label(), theme.dim()));
    if let Some(pct) = state.context_pct {
        left.push(Span::styled(" · ", theme.faint()));
        left.push(Span::styled(context_gauge(pct), theme.faint()));
        left.push(Span::styled(format!(" {pct}% context"), theme.dim()));
    }

    let right = vec![Span::styled(shortcut_hint(state), theme.faint())];
    // Clampé à `area.width - 1` : sur terminal étroit, le segment droit est tronqué
    // plutôt que d'évincer la colonne gauche (workspace/modèle).
    let right_w = (right
        .iter()
        .map(|s| measure::width(s.content.as_ref()))
        .sum::<usize>() as u16
        + INDENT.len() as u16)
        .min(area.width.saturating_sub(1));
    let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(right_w)]).split(area);

    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

fn shortcut_hint(state: &AppState) -> &'static str {
    if state.quit_shortcut_hint_visible() {
        "ctrl+c again to quit"
    } else if matches!(state.status, Status::Thinking) {
        "ctrl+c interrupt"
    } else {
        "ctrl+c twice to quit"
    }
}

fn render_progress_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let line = clip(progress_spans(state, theme), area.width as usize);
    frame.render_widget(Paragraph::new(line), area);
}

fn progress_spans(state: &AppState, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if !state.reduced_motion {
        spans.extend(crate::spinner::shimmer_text(
            "•",
            state.spinner_tick,
            false,
            theme,
        ));
        spans.push(Span::raw(" "));
    }
    spans.extend(crate::spinner::shimmer_text(
        "Working",
        state.spinner_tick,
        state.reduced_motion,
        theme,
    ));
    spans.push(Span::raw(" "));

    let elapsed = state.turn_elapsed.unwrap_or_default();
    spans.push(Span::styled(
        format!(
            "({} • esc to interrupt)",
            crate::spinner::fmt_duration(elapsed)
        ),
        theme.dim(),
    ));

    spans
}

/// Jauge de contexte compacte en 8 cellules (`▰` plein / `▱` vide), arrondie.
fn context_gauge(pct: u8) -> String {
    let filled = ((pct as usize * 8 + 50) / 100).min(8);
    (0..8).map(|i| if i < filled { '▰' } else { '▱' }).collect()
}

fn render_permission(frame: &mut Frame, area: Rect, prompt: &PermissionPrompt, theme: &Theme) {
    let width = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Titre : accent net, sans boîte. Clippé à UNE ligne (hauteur déterministe).
    // Assaini ICI (point de rendu) : le titre porte un `path`/nom d'outil
    // model-controlled qui ne passe PAS par le moteur de diff — sans ça, un `path`
    // contenant de l'OSC/CSI injecterait le terminal (le diff, lui, est déjà assaini).
    let mut title = vec![
        Span::styled("⟐ ", theme.accent()),
        Span::styled(
            sanitize(&prompt.title),
            theme.fg().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  - {}", sanitize(&prompt.reason)), theme.dim()),
    ];
    if let Some(mode) = &prompt.mode {
        title.push(Span::styled(format!(" · {mode}"), theme.faint()));
    }
    if prompt.taint_forced {
        title.push(Span::styled(" · untrusted output", theme.error()));
    }
    lines.push(clip(title, width));

    // Aperçu : MÊME moteur/rendu que le diff inline (US-039). Borné à la place
    // restante (titre + actions réservés) pour que [o]/[n] restent TOUJOURS visibles.
    let mut preview: Vec<Line<'static>> = Vec::new();
    push_diff(&mut preview, &prompt.preview, theme, width, None);
    let room = (area.height as usize).saturating_sub(2);
    if preview.len() <= room {
        lines.extend(preview);
    } else {
        let keep = room.saturating_sub(1);
        let hidden = preview.len() - keep;
        lines.extend(preview.into_iter().take(keep));
        lines.push(Line::from(Span::styled(
            format!("{INDENT}… +{hidden} lines"),
            theme.faint(),
        )));
    }

    lines.push(Line::from(vec![
        Span::styled("  [o]", theme.accent()),
        Span::styled(" allow   ", theme.dim()),
        Span::styled("[n]", theme.accent()),
        Span::styled(" deny", theme.dim()),
    ]));

    // Lignes déjà clippées à la largeur → pas de `Wrap` (hauteur exacte).
    frame.render_widget(Paragraph::new(lines), area);
}

/// Hauteur nécessaire au dialog de permission (titre + aperçu borné + actions).
fn permission_height(prompt: &PermissionPrompt, _width: u16) -> u16 {
    let preview = prompt.preview.rows.len().min(12) as u16;
    (2 + preview).clamp(2, 16)
}

fn truncate(s: &str, max: usize) -> String {
    measure::truncate(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::AgentEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

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

    #[cfg(feature = "codex_tui_parity")]
    fn draw_parity(
        state: &AppState,
        surface: &crate::history_cell::ChatSurface,
        w: u16,
        h: u16,
    ) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_parity(f, state, surface)).unwrap();
        dump(term.backend().buffer())
    }

    // US-019 AC1 : texte streamé rendu token-par-token (markdown), prompt présent.
    #[test]
    fn streamed_text_renders() {
        let mut s = AppState::new("gpt-5", true);
        for tok in ["Bonjour ", "depuis ", "Pyxis"] {
            s.apply(&AgentEvent::Text(tok.into()));
        }
        let out = draw(&s, 40, 12);
        assert!(out.contains("Bonjour depuis Pyxis"), "{out}");
        assert!(out.contains("›"), "prompt de saisie absent");
    }

    // Écran d'accueil : carte avec logo braille (Dyson) + identité, transcript vide.
    #[test]
    fn welcome_card_shows_logo_and_brand() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "pyxis".into();
        s.provider_connected = true;
        assert!(s.is_welcome(), "empty transcript shows welcome");
        let out = draw(&s, 80, 24);
        assert!(out.contains("PYXIS"), "marque absente:\n{out}");
        // Le logo est en points braille (U+2801..=U+28FF, hors blanc U+2800).
        assert!(
            out.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
            "logo braille absent:\n{out}"
        );
        assert!(out.contains("/help"), "raccourcis absents:\n{out}");
        assert!(out.contains("gpt-5.5"), "model missing:\n{out}");
    }

    #[test]
    fn welcome_card_shows_disconnected_state() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "pyxis".into();
        let out = draw(&s, 80, 24);
        assert!(out.contains("not connected"), "auth status missing:\n{out}");
        assert!(
            out.contains("restart pyxis"),
            "reconnection message missing:\n{out}"
        );
    }

    // L'accueil disparaît dès le premier message (transcript non vide).
    #[test]
    fn welcome_disappears_after_first_message() {
        let mut s = AppState::new("gpt-5.5", true);
        s.push_user("hello");
        assert!(!s.is_welcome());
        let out = draw(&s, 80, 24);
        assert!(out.contains("hello"));
        assert!(!out.contains("PYXIS"), "welcome should disappear:\n{out}");
    }

    #[test]
    fn user_block_is_sanitized() {
        let mut s = AppState::new("gpt-5.5", true);
        s.push_user("hello\x1b]0;pwned\x07world");
        let out = draw(&s, 80, 24);
        assert!(!out.contains('\u{1b}'), "ESC residue:\n{out}");
        assert!(out.contains("helloworld"), "sanitized text missing:\n{out}");
    }

    // Terminal trop étroit pour la carte → repli compact, sans panic, marque visible.
    #[test]
    fn welcome_falls_back_compact_on_small_terminal() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "pyxis".into();
        let out = draw(&s, 30, 8);
        assert!(
            out.contains("PYXIS"),
            "compact fallback should keep the brand:\n{out}"
        );
    }

    // Le markdown est rendu, pas affiché en brut (les `**` disparaissent).
    #[test]
    fn markdown_bold_is_not_shown_raw() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("This is **important** here".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 50, 10);
        assert!(out.contains("important"), "{out}");
        assert!(!out.contains("**"), "raw markdown not rendered:\n{out}");
    }

    // US-019 AC2 : un diff avec gouttière (numéros) s'affiche dans le dialog.
    #[test]
    fn permission_dialog_renders_diff_gutter() {
        let mut s = AppState::new("gpt-5", true);
        let preview = crate::diff::from_tool(
            "edit",
            &serde_json::json!({
                "path": "src/main.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"
            }),
        )
        .unwrap();
        s.pending = Some(PermissionPrompt::new(
            "edit src/main.rs",
            "mutation",
            preview,
        ));
        let out = draw(&s, 90, 14);
        assert!(out.contains("allow") && out.contains("deny"), "{out}");
        assert!(out.contains("let x = 1;"), "removed line missing:\n{out}");
        assert!(out.contains("let x = 2;"), "added line missing:\n{out}");
        assert!(out.contains("edit src/main.rs"));
    }

    // Sécurité (US-039) : le titre du dialog (path/nom d'outil model-controlled) est
    // assaini au rendu — un `path` portant de l'OSC/CSI ne fuit pas vers le terminal.
    #[test]
    fn permission_title_is_sanitized() {
        let mut s = AppState::new("gpt-5", true);
        s.pending = Some(PermissionPrompt::new(
            "edit \x1b]0;pwned\x07evil.rs",
            "reason\x1b[31m",
            crate::diff::Diff::default(),
        ));
        let out = draw(&s, 50, 8);
        assert!(!out.contains('\u{1b}'), "ESC residue in dialog:\n{out}");
        assert!(out.contains("evil.rs"), "sanitized title preserved:\n{out}");
        assert!(out.contains("allow"), "actions present:\n{out}");
    }

    // US-019 AC4 : dégradation sans truecolor — pas de panic, layout intact.
    #[test]
    fn monochrome_degradation_renders_without_panic() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("mono text".into()));
        let out = draw(&s, 30, 8);
        assert!(out.contains("mono text"));
    }

    // US-019 AC4 (bis) : terminal étroit → reflow sans corruption (pas de panic).
    #[test]
    fn narrow_terminal_does_not_corrupt() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text(
            "long enough text to wrap across several lines in a narrow terminal".into(),
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
            s.push_user(format!("message number {i} with a little extra text"));
            s.apply(&AgentEvent::Text(format!("answer {i}")));
            s.apply(&AgentEvent::EndTurn);
        }
        // 1er rendu : publie scroll_max (le transcript déborde la fenêtre étroite).
        let _ = draw(&s, 24, 8);
        assert!(
            s.scroll_max.get() > 0,
            "overflowing transcript should set scroll_max"
        );
        // remonter au-delà de la borne est clampé ; le 1er tour devient visible.
        s.scroll_up(1000);
        assert_eq!(s.scroll, s.scroll_max.get(), "scroll clamped to bound");
        let out = draw(&s, 24, 8);
        assert!(
            out.contains("message number 0"),
            "top of transcript should be reachable:\n{out}"
        );
    }

    #[cfg(feature = "codex_tui_parity")]
    #[test]
    fn parity_scroll_reaches_full_transcript() {
        let mut state = AppState::new("gpt-5", true);
        let messages = (0..10)
            .flat_map(|i| {
                [
                    agent_core::Message::user(format!("message {i}")),
                    agent_core::Message::assistant_text(format!("answer {i}")),
                ]
            })
            .collect::<Vec<_>>();
        let surface = crate::history_cell::ChatSurface::from_messages(&messages);

        let bottom = draw_parity(&state, &surface, 48, 10);
        assert!(
            state.scroll_max.get() > 0,
            "parity transcript should publish a scroll bound:\n{bottom}"
        );
        assert!(
            !bottom.contains("message 0"),
            "bottom-pinned parity view should show the transcript tail:\n{bottom}"
        );

        state.scroll_up(1000);
        assert_eq!(state.scroll, state.scroll_max.get());
        let top = draw_parity(&state, &surface, 48, 10);
        assert!(
            top.contains("message 0"),
            "scrolled parity view should render the top of retained transcript:\n{top}"
        );
    }

    #[cfg(feature = "codex_tui_parity")]
    #[test]
    fn parity_idle_composer_is_bottom_anchored() {
        let state = AppState::new("gpt-5", true);
        let surface = crate::history_cell::ChatSurface::from_messages(&[
            agent_core::Message::user("prompt"),
            agent_core::Message::assistant_text("final answer"),
        ]);

        let out = draw_parity(&state, &surface, 48, 12);
        let prompt_row = out
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| line.contains("›").then_some(idx))
            .last()
            .expect("composer prompt should render");
        assert!(
            prompt_row >= 8,
            "idle parity composer should stay near the terminal bottom:\n{out}"
        );
        assert!(
            out.lines()
                .take(prompt_row)
                .any(|line| line.contains("final answer")),
            "transcript tail should remain visible above the bottom composer:\n{out}"
        );
    }

    #[cfg(feature = "codex_tui_parity")]
    #[test]
    fn parity_welcome_is_top_with_bottom_composer() {
        let mut state = AppState::new("gpt-5", true);
        state.workspace = "pyxis".into();
        state.provider_connected = true;
        let surface = crate::history_cell::ChatSurface::new();

        let out = draw_parity(&state, &surface, 80, 20);
        let title_row = out
            .lines()
            .position(|line| line.contains("PYXIS"))
            .expect("welcome title should render");
        let prompt_row = out
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| line.contains("›").then_some(idx))
            .last()
            .expect("composer prompt should render");
        assert!(
            title_row <= 4,
            "welcome should be anchored near the top:\n{out}"
        );
        assert!(
            prompt_row >= 16,
            "welcome composer should be anchored at the bottom:\n{out}"
        );
    }

    #[test]
    fn command_menu_is_windowed_when_items_overflow() {
        let mut s = AppState::new("gpt-5", true);
        s.skills = (0..20).map(|i| format!("skill-{i:02}")).collect();
        s.set_input("/skills ".into());

        let out = draw(&s, 90, 14);
        assert!(out.contains("skill-00"), "{out}");
        assert!(out.contains("1-8/20"), "range absent:\n{out}");

        for _ in 0..10 {
            s.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let out = draw(&s, 50, 14);
        assert!(out.contains("skill-10"), "{out}");
        assert!(out.contains("4-11/20"), "window did not scroll:\n{out}");
    }

    // Refus de permission interrompt proprement (état nettoyé) — AC3.
    #[test]
    fn refusing_permission_clears_prompt() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = AppState::new("gpt-5", true);
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "sensible",
            crate::diff::Diff::default(),
        ));
        let action = s.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, crate::state::InputAction::Permission(false));
        assert!(s.pending.is_none());
    }

    // US-034 : un tour assistant est ancré par une puce ●.
    #[test]
    fn assistant_turn_has_bullet_anchor() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("Bonjour".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 40, 8);
        assert!(out.contains('●'), "puce d'ancrage absente:\n{out}");
        assert!(out.contains("Bonjour"));
    }

    // US-034 : un tour assistant VIDE ne laisse pas de puce orpheline.
    #[test]
    fn empty_assistant_has_no_orphan_bullet() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("salut");
        s.apply(&AgentEvent::Text(String::new()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 40, 8);
        assert!(out.contains("salut"));
        assert!(!out.contains('●'), "puce orpheline sur tour vide:\n{out}");
    }

    // US-035 : un edit affiche le label Update(path) + résumé ⎿ Added/removed.
    #[test]
    fn edit_tool_shows_label_and_summary() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolCall(agent_core::event::ToolCallView {
            id: "c1".into(),
            name: "edit".into(),
            input: serde_json::json!({
                "path": "src/main.rs", "old_string": "a\nb", "new_string": "x\ny\nz"
            }),
        }));
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "c1".into(),
            content: "Edited: src/main.rs (level 1: exact)".into(),
            is_error: false,
            untrusted: false,
            error_kind: None,
        }));
        let out = draw(&s, 60, 16);
        assert!(out.contains("Update("), "label Update absent:\n{out}");
        assert!(out.contains("src/main.rs"));
        assert!(out.contains('⎿'), "connecteur ⎿ absent:\n{out}");
        assert!(
            out.contains("Added") && out.contains("removed"),
            "diff summary missing:\n{out}"
        );
    }

    // US-035 : une lecture affiche un résumé condensé ⎿ Read N lines.
    #[test]
    fn read_tool_shows_line_count() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolCall(agent_core::event::ToolCallView {
            id: "r1".into(),
            name: "read".into(),
            input: serde_json::json!({ "path": "a.rs" }),
        }));
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "r1".into(),
            content: "     1\tfn main() {\n     2\t}\n".into(),
            is_error: false,
            untrusted: true,
            error_kind: None,
        }));
        let out = draw(&s, 50, 10);
        assert!(out.contains("Read"), "verbe Read absent:\n{out}");
        assert!(out.contains("lines"), "line count missing:\n{out}");
    }

    // US-036 : une erreur d'outil est rendue avec le préfixe Error:.
    #[test]
    fn tool_error_uses_error_grammar() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "x1".into(),
            content: "anchor not found in src/x.rs".into(),
            is_error: true,
            untrusted: true,
            error_kind: None,
        }));
        let out = draw(&s, 60, 8);
        assert!(out.contains("Error:"), "error grammar missing:\n{out}");
        assert!(out.contains("anchor not found"));
    }

    // US-036 : un rejet utilisateur est distinct d'une erreur (pas de « Error: »).
    #[test]
    fn user_rejection_is_not_an_error() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "x2".into(),
            content: "action \"edit\" rejected by user".into(),
            is_error: true,
            untrusted: false,
            error_kind: Some(agent_core::ToolErrorKind::PermissionDenied),
        }));
        let out = draw(&s, 64, 8);
        assert!(out.contains("rejected"), "rejection label missing:\n{out}");
        assert!(
            !out.contains("Error:"),
            "rejection should not render as an error:\n{out}"
        );
    }

    // Sécurité (US-036 / FR-10) : `sanitize` neutralise TOUTES les familles
    // d'échappement, pas seulement CSI — OSC (titre/hyperlink/clipboard), DCS, C1
    // 8 bits et DEL — sur une sortie d'outil/modèle adverse.
    #[test]
    fn sanitize_strips_all_escape_families() {
        // CSI (déjà couvert) + OSC terminé par BEL.
        assert_eq!(sanitize("a\x1b[31mb\x1b]0;titre\x07c"), "abc");
        // OSC 8 (hyperlink) terminé par ST (ESC \).
        assert_eq!(sanitize("x\x1b]8;;http://evil\x1b\\y"), "xy");
        // DCS terminé par ST.
        assert_eq!(sanitize("p\x1bPq…data\x1b\\r"), "pr");
        // C1 8 bits (CSI/OSC 0x9B/0x9D) et DEL retirés.
        assert_eq!(sanitize("u\u{9b}31mv\u{7f}w"), "uvw");
        // ESC nu en fin de chaîne : pas de panic, simplement avalé.
        assert_eq!(sanitize("fin\x1b"), "fin");
        // Aucun ESC résiduel quel que soit le payload.
        let dirty = "\x1b]0;\x07\x1b[1m\u{9d}\x7f\x1bc texte";
        assert!(!sanitize(dirty).contains('\u{1b}'), "ESC residue");
    }

    // US-038 : un edit réussi affiche le diff coloré (lignes +/-) sous le résumé.
    #[test]
    fn inline_diff_shows_after_successful_edit() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolCall(agent_core::event::ToolCallView {
            id: "c1".into(),
            name: "edit".into(),
            input: serde_json::json!({
                "path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"
            }),
        }));
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "c1".into(),
            content: "Edited: a.rs (level 1)".into(),
            is_error: false,
            untrusted: false,
            error_kind: None,
        }));
        let out = draw(&s, 60, 12);
        assert!(out.contains("let x = 1;"), "removed line missing:\n{out}");
        assert!(out.contains("let x = 2;"), "added line missing:\n{out}");
        assert!(
            out.contains('+') && out.contains('-'),
            "signes de diff absents:\n{out}"
        );
    }

    // US-038 : un edit ÉCHOUÉ n'affiche aucun diff (seulement l'erreur).
    #[test]
    fn failed_edit_shows_no_diff() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolCall(agent_core::event::ToolCallView {
            id: "c1".into(),
            name: "edit".into(),
            input: serde_json::json!({ "path": "a.rs", "old_string": "ZZZ", "new_string": "YYY" }),
        }));
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "c1".into(),
            content: "anchor not found in a.rs".into(),
            is_error: true,
            untrusted: true,
            error_kind: None,
        }));
        let out = draw(&s, 60, 10);
        assert!(out.contains("Error:"), "error missing:\n{out}");
        assert!(
            !out.contains("YYY"),
            "no diff should render for a failed edit:\n{out}"
        );
    }

    // US-039 : un diff de permission très long est tronqué SANS masquer [o]/[n].
    #[test]
    fn permission_dialog_keeps_actions_visible_on_long_diff() {
        let mut s = AppState::new("gpt-5", true);
        let content = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = crate::diff::from_tool(
            "write",
            &serde_json::json!({ "path": "big.rs", "content": content }),
        )
        .unwrap();
        s.pending = Some(PermissionPrompt::new("write big.rs", "creation", preview));
        let out = draw(&s, 50, 20);
        assert!(
            out.contains("allow") && out.contains("deny"),
            "actions hidden by a long diff:\n{out}"
        );
        assert!(out.contains("lines"), "truncation marker missing:\n{out}");
    }

    // US-041 : le cache ne reconstruit que le bloc qui change ; un resize invalide
    // tout. (Le compteur `render_rebuilds` instrumente la passe précédente.)
    #[test]
    fn cache_rebuilds_only_changed_blocks() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("Bonjour".into()));
        s.apply(&AgentEvent::EndTurn);
        s.push_user("question");
        s.apply(&AgentEvent::Text("Réponse en **gras**".into()));

        // Frame 1 : cache froid → les 3 blocs sont construits.
        let _ = draw(&s, 60, 20);
        assert_eq!(s.render_rebuilds(), 3, "1re frame : tout construit");

        // Frame 2 : transcript inchangé → 100 % cache hit.
        let _ = draw(&s, 60, 20);
        assert_eq!(s.render_rebuilds(), 0, "blocs baked servis depuis le cache");

        // Un token arrive sur le dernier bloc (stream) → une seule reconstruction.
        s.apply(&AgentEvent::Text(" et suite".into()));
        let _ = draw(&s, 60, 20);
        assert_eq!(
            s.render_rebuilds(),
            1,
            "seul le bloc en stream est reconstruit"
        );

        // Resize (reflow) → cache invalidé → tout reconstruit.
        let _ = draw(&s, 40, 20);
        assert_eq!(s.render_rebuilds(), 3, "le resize invalide tout le cache");
    }

    // US-043 : une table markdown est rendue alignée dans le transcript (la largeur
    // de contenu est correctement transmise à `render_markdown`).
    #[test]
    fn markdown_table_renders_in_transcript() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        s.apply(&AgentEvent::Text(
            "| Col A | Col B |\n|---|---|\n| 1 | 2 |\n".into(),
        ));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 60, 20);
        assert!(
            out.contains("Col A") && out.contains("Col B"),
            "en-tête de table absente:\n{out}"
        );
        assert!(out.contains('│'), "séparateur de colonnes absent:\n{out}");
    }

    // US-042 : la coloration du diff préserve l'emphase word-diff et applique la
    // teinte de syntaxe aux segments non emphasés, sans masquer le fond.
    #[test]
    fn diff_segs_spans_preserves_emphasis_and_applies_syntax() {
        let theme = Theme::new(true);
        let segs = vec![
            crate::diff::Seg {
                text: "let ".into(),
                emphasized: false,
            },
            crate::diff::Seg {
                text: "x".into(),
                emphasized: true,
            },
        ];
        // Sans coloration : texte intact, l'emphase porte le style saturé `word`.
        let spans = diff_segs_spans(&segs, None, theme.diff_add(), theme.diff_add_word());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "let x");
        assert!(
            spans
                .iter()
                .any(|s| s.content.as_ref() == "x" && s.style == theme.diff_add_word()),
            "emphasized segment should keep word-diff style"
        );

        // Avec une couleur par caractère : les non-emphasés prennent la teinte fournie
        // (fg) tout en gardant le fond `base` ; l'emphasé reste `word`.
        let colors = vec![Color::Rgb(1, 2, 3); joined.chars().count()];
        let spans2 = diff_segs_spans(
            &segs,
            Some(&colors),
            theme.diff_add(),
            theme.diff_add_word(),
        );
        assert!(
            spans2
                .iter()
                .any(|s| s.style.fg == Some(Color::Rgb(1, 2, 3))),
            "syntax tint should apply to non-emphasized segments"
        );
        assert!(
            spans2
                .iter()
                .any(|s| s.content.as_ref() == "x" && s.style == theme.diff_add_word()),
            "word-diff emphasis takes priority over syntax coloring"
        );
    }

    // US-042 (robustesse) : l'alignement couleur↔caractère du diff tient en
    // multi-octets (sinon la teinte se désaligne après le 1er char accentué).
    #[test]
    fn diff_segs_spans_aligns_colors_with_multibyte() {
        let theme = Theme::new(true);
        let segs = [crate::diff::Seg {
            text: "let tea = 1; // ☕".into(),
            emphasized: false,
        }];
        let line: String = segs.iter().map(|s| s.text.as_str()).collect();
        let colors = vec![Color::Rgb(9, 9, 9); line.chars().count()];
        let spans = diff_segs_spans(
            &segs,
            Some(&colors),
            theme.diff_add(),
            theme.diff_add_word(),
        );
        // Texte reconstruit intact ET chaque caractère tinté (aucun retour à `base`
        // faute d'alignement multi-octet).
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, line);
        assert!(
            spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Rgb(9, 9, 9))),
            "complete tint, no misalignment on multibyte characters"
        );
    }

    // US-044/045 : pendant un tour, une ligne Codex-like s'affiche au-dessus du composer.
    #[test]
    fn progress_shows_working_status_above_composer() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        s.apply(&AgentEvent::Text(
            "answer long enough to estimate tokens".into(),
        ));
        s.tick_progress(std::time::Duration::from_secs(3));
        let out = draw(&s, 80, 12);
        assert!(out.contains("Working"), "working status missing:\n{out}");
        assert!(out.contains("3s"), "duration missing:\n{out}");
        assert!(
            out.contains("esc to interrupt"),
            "interrupt hint missing:\n{out}"
        );
        assert!(
            !out.contains('~'),
            "Codex-like status should not show token estimate:\n{out}"
        );
        let status_row = out
            .lines()
            .position(|line| line.contains("Working"))
            .expect("status row should render");
        let prompt_row = out
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| line.contains("›").then_some(idx))
            .last()
            .expect("composer prompt should render");
        assert!(
            status_row < prompt_row,
            "working status should render above the composer:\n{out}"
        );
        let rows = out.lines().collect::<Vec<_>>();
        assert!(
            rows.get(status_row + 1)
                .is_some_and(|line| line.trim().is_empty()),
            "working status should breathe before the composer:\n{out}"
        );
    }

    // US-045 : à la fin du tour, les indicateurs de droite disparaissent.
    #[test]
    fn idle_footer_omits_ready_state() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        s.apply(&AgentEvent::Text("answer".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 80, 12);
        assert!(out.contains("gpt-5"), "model expected in footer:\n{out}");
        assert!(!out.contains("ready"), "idle state too verbose:\n{out}");
    }

    #[test]
    fn footer_shows_double_ctrl_c_quit_hint() {
        let s = AppState::new("gpt-5", true);
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("ctrl+c twice to quit"),
            "double ctrl+c hint missing:\n{out}"
        );
        assert!(
            !out.contains("^C quit"),
            "footer should not advertise single ctrl+c quit:\n{out}"
        );
    }

    #[test]
    fn footer_shows_ctrl_c_interrupt_while_running() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("ctrl+c interrupt"),
            "ctrl+c interrupt hint missing:\n{out}"
        );
    }

    #[test]
    fn footer_shows_ctrl_c_again_after_first_press() {
        let mut s = AppState::new("gpt-5", true);
        s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("ctrl+c again to quit"),
            "ctrl+c again hint missing:\n{out}"
        );
    }

    #[test]
    fn shutdown_feedback_replaces_composer_and_hides_footer() {
        let mut s = AppState::new("gpt-5", true);
        s.set_input("draft".into());
        s.show_shutdown_in_progress();
        let out = draw(&s, 80, 8);
        assert!(
            out.contains("› Shutting down..."),
            "shutdown placeholder missing:\n{out}"
        );
        assert!(!out.contains("draft"), "draft should be hidden:\n{out}");
        assert!(
            !out.contains("ctrl+c"),
            "footer hint should be hidden:\n{out}"
        );
        assert!(
            !out.contains("gpt-5"),
            "status line should be hidden:\n{out}"
        );
    }

    // US-046 : la pill « nouveaux messages » n'apparaît QUE remonté ET contenu arrivé.
    #[test]
    fn scroll_pill_only_when_scrolled_up_with_unseen() {
        let mut s = AppState::new("gpt-5", true);
        for i in 0..30 {
            s.push_user(format!("q{i}"));
            s.apply(&AgentEvent::Text(format!("answer {i}")));
            s.apply(&AgentEvent::EndTurn);
        }
        // Collé en bas (scroll == 0) : pas de pill.
        let bottom = draw(&s, 60, 10);
        assert!(
            !bottom.contains("new"),
            "no pill while pinned to bottom:\n{bottom}"
        );
        // L'utilisateur remonte (scroll_max posé par le draw précédent), du contenu arrive.
        s.scroll_up(3);
        s.apply(&AgentEvent::Text("fresh content outside the view".into()));
        let up = draw(&s, 60, 10);
        assert!(
            up.contains("new"),
            "pill expected after scroll plus content:\n{up}"
        );
    }

    // US-044 (robustesse) : la ligne de progression ne panique pas en terminal étroit.
    #[test]
    fn progress_status_line_survives_narrow_terminal() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        s.apply(&AgentEvent::Text("answer".into()));
        s.tick_progress(std::time::Duration::from_secs(3));
        // Largeur 8 : le draw doit aboutir (pas de panic, pas de corruption).
        let out = draw(&s, 8, 6);
        assert!(!out.is_empty());
    }
}
