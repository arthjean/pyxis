//! Ratatui rendering (US-019). Aesthetics: **monochrome + one accent**, spare,
//! no heavy border. Hierarchy through weight/tint and negative space, not through
//! color. Visual signature: a `▌` gutter that lights up (accent) on the
//! assistant turn being streamed, and calms down (faint) once finished.
//!
//! `render` is PURE -> testable through `TestBackend`. Degradation without truecolor
//! (AC4) replaces the accent with bold; the layout is unchanged.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as Boundary, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;

use agent_core::ToolErrorKind;

use crate::cache::fingerprint;
use crate::composer;
use crate::footer::{self, FooterProps, StatusSegment};
use crate::measure;
use crate::state::{
    AppState, Block, COMMANDS, DEFAULT_PERMISSION_MODE_ID, MenuItem, PermissionPrompt, Status,
};
use crate::theme::Theme;
use crate::tool;

const INDENT: &str = "  ";
/// Composer collapsed when the session stops: rule + line + rule.
const SHUTDOWN_INPUT_HEIGHT: u16 = 3;
/// Invitation shown in place of an empty draft (Codex `Ask Codex to do anything`).
const COMPOSER_PLACEHOLDER: &str = "Ask Pyxis to do anything";
/// Height cap of the composer, in text lines (US-010 AC2). Past it, the
/// area scrolls to keep the cursor line visible.
const COMPOSER_MAX_ROWS: u16 = 10;
/// Composer gutter: `› ` on the first line, alignment on the others.
const COMPOSER_GUTTER: u16 = 2;
const PROGRESS_HEIGHT: u16 = 1;
const PROGRESS_GAP_HEIGHT: u16 = 1;
const MENU_MAX_ITEMS: u16 = 8;

/// Full rendering of a frame.
pub fn render(frame: &mut Frame, state: &AppState) {
    let theme = Theme::new(state.truecolor);
    let area = frame.area();

    // At the bottom: either the permission dialog, or (status + input). Clamped to
    // leave at least one transcript line when the terminal is shorter
    // than what the composer asks for (US-010 AC6).
    let bottom_height = match &state.pending {
        Some(p) => permission_height(p, area.width),
        None => input_height(state, area.width),
    }
    .min(area.height.saturating_sub(1));
    // Slash command menu: popup inserted between transcript and input (never
    // during a permission dialog). +1 line for the shortcut reminder.
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

    // Empty transcript -> welcome screen (card + pixel logo), otherwise the normal thread.
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
pub(crate) fn render_parity(
    frame: &mut Frame,
    state: &AppState,
    surface: &crate::history_cell::ChatSurface,
) {
    let theme = Theme::new(state.truecolor);
    let area = frame.area();

    if state.transcript_overlay_open() {
        render_transcript_overlay(frame, area, state, surface, &theme);
        return;
    }

    let bottom_height = match &state.pending {
        Some(p) => permission_height(p, area.width),
        None => input_height(state, area.width),
    }
    .min(area.height.saturating_sub(1));
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

#[cfg(feature = "codex_tui_parity")]
fn render_transcript_overlay(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    surface: &crate::history_cell::ChatSurface,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    if area.width == 0 || area.height == 0 {
        state.set_transcript_overlay_metrics(0, 1);
        return;
    }

    let top_height = area.height.saturating_sub(3);
    let top = Rect::new(area.x, area.y, area.width, top_height);
    let hints = Rect::new(
        area.x,
        area.y + top_height,
        area.width,
        area.height - top_height,
    );

    render_transcript_overlay_view(frame, top, state, surface, theme);
    render_transcript_overlay_hints(frame, hints, theme);
}

#[cfg(feature = "codex_tui_parity")]
fn render_transcript_overlay_view(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    surface: &crate::history_cell::ChatSurface,
    theme: &Theme,
) {
    if area.height == 0 {
        state.set_transcript_overlay_metrics(0, 1);
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let header_fill = "/ ".repeat((area.width as usize).saturating_add(1) / 2);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(header_fill, theme.faint()))),
        header,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "/ T R A N S C R I P T",
            theme.dim(),
        ))),
        header,
    );

    let separator_y = area.bottom().saturating_sub(1);
    let content = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        separator_y.saturating_sub(area.y.saturating_add(1)),
    );
    state.set_transcript_overlay_metrics(0, content.height);

    let all_lines = surface.transcript_lines(content.width);
    let max_off = all_lines.len().saturating_sub(content.height as usize);
    state.set_transcript_overlay_metrics(max_off, content.height);
    let scroll = state.transcript_overlay_scroll().min(max_off);
    let offset = max_off.saturating_sub(scroll);
    let visible = if content.height == 0 {
        Vec::new()
    } else {
        all_lines
            .into_iter()
            .skip(offset)
            .take(content.height as usize)
            .collect()
    };
    frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), content);

    let separator = Rect::new(area.x, separator_y, area.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(separator.width as usize),
            theme.faint(),
        ))),
        separator,
    );
    render_transcript_overlay_percent(frame, separator, offset, max_off, theme);
}

#[cfg(feature = "codex_tui_parity")]
fn render_transcript_overlay_percent(
    frame: &mut Frame,
    area: Rect,
    offset: usize,
    max_off: usize,
    theme: &Theme,
) {
    if area.width == 0 {
        return;
    }
    let percent = if max_off == 0 {
        100
    } else {
        ((offset as f32 / max_off as f32) * 100.0).round() as u8
    };
    let text = format!(" {percent}% ");
    let width = (measure::width(&text) as u16).min(area.width);
    let x = area.x + area.width.saturating_sub(width);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, theme.dim()))).alignment(Alignment::Right),
        Rect::new(x, area.y, width, 1),
    );
}

#[cfg(feature = "codex_tui_parity")]
fn render_transcript_overlay_hints(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let lines = [
        vec![
            Span::styled(" ↑/↓", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" or ", theme.faint()),
            Span::styled("j/k", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" scroll   ", theme.dim()),
            Span::styled("PgUp/PgDn", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" page   ", theme.dim()),
            Span::styled("Home/End", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" jump", theme.dim()),
        ],
        vec![
            Span::styled(" ctrl+t", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" close   ", theme.dim()),
            Span::styled("q", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" close   ", theme.dim()),
            Span::styled("ctrl+c", theme.fg().add_modifier(Modifier::BOLD)),
            Span::styled(" close", theme.dim()),
        ],
    ];

    for (idx, spans) in lines.into_iter().enumerate().take(area.height as usize) {
        let line_area = Rect::new(area.x, area.y + idx as u16, area.width, 1);
        frame.render_widget(Paragraph::new(clip(spans, area.width as usize)), line_area);
    }
}

/// Pyxis logo: a minimalist **Dyson sphere**. The compass gives the heading
/// in a vast space; here, a crisp stellar core ringed by two rings of
/// collectors (with gaps, the swarm still assembling). Rendered in **dithered
/// braille dots** (stippling, pixel-dust style), monochrome; the sky-blue accent
/// stays reserved for the UI. Continuous field
/// (resolution-independent), not a frozen bitmap.
const LOGO_COLS: usize = 20;
const LOGO_ROWS: usize = 10;
/// Ring thickness / core size / dot density ("11d" tuning).
const LOGO_LINE_W: f32 = 0.11;
const LOGO_CORE_W: f32 = 0.15;
const LOGO_GAMMA: f32 = 0.7;

/// 4x4 Bayer matrix (ordered dithering): converts the field intensity into
/// dot density (the "more or less packed" look).
const LOGO_BAYER: [[f32; 4]; 4] = [
    [0.0, 8.0, 2.0, 10.0],
    [12.0, 4.0, 14.0, 6.0],
    [3.0, 11.0, 1.0, 9.0],
    [15.0, 7.0, 13.0, 5.0],
];

/// Layout of the 8 dots of a braille cell -> bit (base U+2800).
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

/// Continuous field of the Dyson sphere in normalized coordinates nx,ny in [-1,1]
/// (radius 1 = edge): gaussian stellar core + two thin tilted rings, each with
/// a gap. Returns an intensity 0.0 (empty) .. 1.0 (core).
fn logo_field(nx: f32, ny: f32) -> f32 {
    use std::f32::consts::TAU;
    let rn = (nx * nx + ny * ny).sqrt();
    let core = (-(rn / LOGO_CORE_W).powi(2)).exp();
    // (tilt, minor axis ratio, gap start, gap end) in radians.
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

/// Renders the logo field as dithered braille dots (2x4 subdots per cell).
/// Density follows the intensity boosted by `LOGO_GAMMA` (< 1 = more dots,
/// true background preserved). Monochrome: theme grey depending on the cell peak;
/// without truecolor, falls back on `fg`.
fn logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    let (cols, rows) = (LOGO_COLS, LOGO_ROWS);
    let (sw, sh) = (cols * 2, rows * 4); // square subgrid (cols = 2*rows)
    let scale = 1.05_f32; // slight play around the logo
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
                // Grey in a middle band (neither too dark, nor pure white).
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

/// Welcome screen (Grok-style hero): a card at the top, braille logo (Dyson
/// sphere, monochrome) on the left, identity + shortcuts on the right. Displayed as long
/// as no conversation has started (`AppState::is_welcome`); the input stays
/// rendered below, unchanged. Compact fallback (without logo nor border) when the terminal
/// is too narrow for the full card.
fn render_welcome(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // No transcript to scroll on the welcome screen.
    state.scroll_max.set(0);

    let logo = logo_lines(theme);
    let logo_w = logo.iter().map(|l| l.width()).max().unwrap_or(0) as u16;

    // Right column: identity, meta (model/workspace/provider), shortcuts.
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
    if let Some(effort) = &state.reasoning_effort
        && !effort.trim().is_empty()
    {
        meta.push(Span::styled(format!(" [{}]", effort.trim()), theme.faint()));
    }
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
        Span::styled("/effort", theme.accent()),
    ]));
    info.push(Line::from(vec![
        Span::styled("/permissions", theme.accent()),
        Span::styled("  ·  ", theme.faint()),
        Span::styled("/goal", theme.accent()),
    ]));
    // The footer spends its row on the status line, so `?` is announced here:
    // without it the shortcut overlay would be unreachable by discovery.
    info.push(Line::from(Span::styled(
        "? for shortcuts   ·   ↑ history",
        theme.faint(),
    )));

    let info_w = info.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let gap: u16 = 3; // breathing column between logo and text
    let pad: u16 = 2; // inner horizontal margin (on both sides)
    let inner_w = logo_w + gap + info_w;
    let inner_h = logo.len().max(info.len()) as u16;
    let card_w = inner_w + pad * 2 + 2; // + 2 borders
    let card_h = inner_h + 4; // 2 margin lines (top/bottom) + 2 borders

    // Terminal too small for the full card -> compact fallback.
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

    // Composes each line: logo (left) + gap + info (right), both blocks
    // vertically centered in `inner_h`.
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

    // 1 margin line at the top, `pad` columns on the left, inside the frame.
    let body = Rect {
        x: content.x + pad,
        y: content.y + 1,
        width: content.width.saturating_sub(pad),
        height: content.height.saturating_sub(1),
    };
    frame.render_widget(Paragraph::new(rows), body);
}

/// Welcome fallback for a narrow terminal: the identity block alone at the top,
/// without logo nor border (avoids truncating the card).
fn render_welcome_compact(frame: &mut Frame, area: Rect, info: &[Line<'static>]) {
    let w = info.iter().map(|l| l.width()).max().unwrap_or(1).max(1) as u16;
    let h = (info.len() as u16).max(1);
    let rect = top_centered_rect(area, w, h);
    frame.render_widget(Paragraph::new(info.to_vec()), rect);
}

/// Slash completion menu (Grok style): one line per item (aligned label +
/// faint hint), the selection on a highlighted background with a `›`. Serves every
/// submenu (commands, models, sessions, providers): inactive items are
/// greyed out, a `✓` hint (connected) turns to the accent, long labels are cut.
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
        // Inactive -> greyed out; active -> fg (bold when selected).
        let name_st = if !item.enabled {
            theme.faint()
        } else if selected {
            theme.fg().add_modifier(Modifier::BOLD)
        } else {
            theme.fg()
        };
        // Status badge: ✓ connected (accent), ✗ failed (error), ◯ in progress
        // (dim), other hints muted.
        let hint_st = if item.hint.starts_with('✓') {
            theme.accent()
        } else if item.hint.starts_with('✗') {
            theme.error()
        } else if item.hint.starts_with('◯') {
            theme.dim()
        } else {
            theme.faint()
        };
        let safe_label = sanitize(&item.label);
        let safe_hint = sanitize(&item.hint);
        let name_disp = fit(&safe_label, namecol);
        let desc_room = width.saturating_sub(2 + namecol + 2).max(1);
        let desc_disp = fit(&safe_hint, desc_room);
        let desc_len = measure::width(&desc_disp);
        let mut spans = vec![
            Span::styled(marker, marker_st),
            Span::styled(measure::pad_right(name_disp, namecol), name_st),
            Span::raw("  "),
            Span::styled(desc_disp, hint_st),
        ];
        // Fills the end of the line to spread the highlighted background over the full width.
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

/// Truncates `s` to `width` columns (ellipsis `…` on overflow).
fn fit(s: &str, width: usize) -> String {
    measure::truncate(s, width)
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let width = area.width as usize;
    // Index of tool calls by id: pairs a ToolResult with its ToolCall
    // (US-033) to derive the `⎿` summary from the call input.
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

    // Cache of styled lines per block (US-041): we only rebuild (markdown parse
    // + coloring) the blocks whose fingerprint changed, typically the single
    // block being streamed. The others are served from the cache. `render` stays
    // pure: the cache lives in interior mutability on `AppState`.
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
        // An empty assistant turn (before the 1st token, or purely blank text) renders
        // zero lines -> no orphan bullet (US-034) and `prev` stays unchanged.
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

    // US-015: output of the still-running tool, under its call. Outside the cache: this
    // buffer changes on every fragment, and it disappears as soon as the result arrives.
    for (i, line) in state.live_output_lines().into_iter().enumerate() {
        // Same gutter as the result summary (`⎿`): the live preview visually
        // occupies the place the final output will take.
        let prefix = if i == 0 {
            Span::styled("  ⎿ ", theme.faint())
        } else {
            Span::raw("    ")
        };
        push_wrapped(
            &mut lines,
            vec![Span::styled(line, theme.dim())],
            prefix,
            Span::raw("    "),
            width,
        );
    }

    // The manual wrap above lays out the hanging gutter (bullet + 2-col indent) for
    // the common case (width counted in `char`). We keep `Wrap` as a SAFETY NET: a
    // line that would exceed the width in COLUMNS (wide CJK/emoji chars, which the
    // `char` count does not see) is re-wrapped by ratatui rather than TRUNCATED
    // (no loss). The scroll bound is therefore computed on the lines AFTER wrapping.
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

/// Discreet "new messages" pill (US-046): at the bottom of the transcript when
/// the user has scrolled up the thread AND content arrived below.
/// Right-aligned, bounded to the width (does not overflow, does not hide the input, which
/// lives in a separate area). `⇟` = shortcut to scroll back down.
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

/// Is a blank line needed BEFORE this block? We group tools (consecutive
/// call/result kept together) and give the rest some air.
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
            // Rendered markdown, ANCHORED by a sky-blue bullet; body aligned at 2 columns
            // (hanging gutter: bullet on the 1st subline, rest indented). The
            // CONTENT width (gutter excluded) sizes the markdown tables
            // (US-043), the same value as the `emit_block` wrap.
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
            // Collapsed into a discreet label; while in progress (last block), a short
            // preview of the last thought lines ("Thinking..." style).
            lines.push(Line::from(vec![
                Span::styled(format!("{INDENT}· "), theme.faint()),
                Span::styled("thinking", theme.faint().add_modifier(Modifier::ITALIC)),
            ]));
            if is_last {
                let preview_st = theme.faint().add_modifier(Modifier::ITALIC);
                let cont = Span::styled(format!("{INDENT}  "), theme.faint());
                for raw in preview_tail(&sanitize(text), 2) {
                    // Through `push_wrapped` (like any other block): the gutter
                    // hanging at 4 columns survives the wrap on a narrow terminal
                    // (otherwise the 2nd subline comes back to column 0, US-034).
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
            // Grey bullet + structured `Verb(target)` label (US-035).
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
            let call = calls
                .get(call_id.as_str())
                .map(|(name, input, _)| (*name, *input));
            if *is_error {
                if matches!(error_kind, Some(ToolErrorKind::PermissionDenied))
                    || tool::is_user_rejection(content)
                {
                    // Deliberate rejection (permission refused): softened tone, not
                    // red: this is not a system error (US-036).
                    push_wrapped(
                        lines,
                        vec![Span::styled(tool::reject_summary(content), theme.dim())],
                        Span::styled(format!("{INDENT}⎿ "), theme.dim()),
                        Span::styled(format!("{INDENT}  "), theme.dim()),
                        width,
                    );
                } else {
                    // US-019 AC2: a failed Code Mode cell states `failed` in
                    // words BEFORE the message. Red is what a screen reader and
                    // a monochrome terminal do not get; the word is what both
                    // do. The state is then REMOVED from the message so it is
                    // not printed a second time as the error's first line.
                    let split = call
                        .filter(|(name, _)| matches!(*name, "exec" | "wait"))
                        .and_then(|_| tool::cell_state_split(content));
                    let detail = match &split {
                        Some((state, rest)) => {
                            push_wrapped(
                                lines,
                                vec![Span::styled(state.clone(), theme.error())],
                                Span::styled(format!("{INDENT}⎿ "), theme.error()),
                                Span::styled(format!("{INDENT}  "), theme.error()),
                                width,
                            );
                            rest.as_str()
                        }
                        None => content.as_str(),
                    };
                    // Tool error: connector + red message, bounded to 1 line
                    // + indicator of the rest (US-036).
                    push_wrapped(
                        lines,
                        vec![Span::styled(tool::error_summary(detail), theme.error())],
                        Span::styled(format!("{INDENT}⎿ "), theme.error()),
                        Span::styled(format!("{INDENT}  "), theme.error()),
                        width,
                    );
                    let extra = tool::extra_lines(detail);
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
                // Secondary `⎿` summary (numbers highlighted) paired with the call.
                push_wrapped(
                    lines,
                    tool::result_summary(call, content, theme),
                    Span::styled(format!("{INDENT}⎿ "), theme.faint()),
                    Span::styled(format!("{INDENT}  "), theme.faint()),
                    width,
                );
                // Inline diff (US-038): successful edit/write -> diff derived from the call
                // input (nothing for reads nor for non-mutating tools).
                if let Some((name, input)) = call
                    && let Some(d) = crate::diff::from_tool(name, input)
                {
                    // Syntax coloring of the diff (US-042): language inferred from
                    // the extension of the edited path.
                    let lang = input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .and_then(crate::highlight::lang_from_path);
                    push_diff(lines, &d, theme, width, lang.as_deref());
                }
            }
        }
        Block::Plan(view) => {
            // US-009: one line per step, its glyph carrying the state. The step
            // in progress is the only one at full contrast: a plan is read to
            // find out where the agent stands.
            if let Some(explanation) = &view.explanation {
                push_wrapped(
                    lines,
                    vec![Span::styled(explanation.clone(), theme.dim())],
                    Span::styled(format!("{INDENT}· "), theme.dim()),
                    Span::styled(format!("{INDENT}  "), theme.dim()),
                    width,
                );
            }
            for step in &view.steps {
                let style = match step.status {
                    agent_core::PlanStatus::InProgress => theme.fg(),
                    _ => theme.dim(),
                };
                push_wrapped(
                    lines,
                    vec![Span::styled(step.step.clone(), style)],
                    Span::styled(
                        format!("{INDENT}{} ", crate::state::plan_status_glyph(step.status)),
                        style,
                    ),
                    Span::styled(format!("{INDENT}  "), theme.dim()),
                    width,
                );
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

/// Emits a markdown block (several logical lines) anchored by `bullet` on the
/// very first subline; the others are indented at 2 columns (hanging gutter
/// that survives the wrap). Empty block -> nothing (no orphan bullet, US-034).
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

/// Pushes a logical line `content` wrapped at `width`, `first` heading the 1st
/// subline and `cont` the following ones (prefixes of equal width -> clean
/// alignment). Preserves the styles; cuts words that are too long.
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

/// Word-wraps a span sequence at `width` terminal columns, styles preserved.
/// Cuts at the last space; failing that (word longer than `width`), hard cut.
/// Returns at least one subline (possibly empty).
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
                line.pop(); // drops the cutting space
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

/// Recomposes a `(grapheme, style)` sequence into spans, merging runs of the same style.
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

/// Cleans up a model text: drops CR, ANSI sequences (CSI) and C0 controls,
/// the residues that used to "leak" to the right, and converts tabs into spaces.
/// Renders a structured diff (US-038) under the `⎿` summary: gutter of (relative)
/// line numbers, `+`/`-` sign, green/red backgrounds in truecolor (or sign + bold/dim in
/// 16 colors), saturated word-by-word emphasis. Lines that are too wide are truncated
/// without corrupting the gutter (which stays at the head of the line).
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

/// Syntax colors (one per character) of a diff line rebuilt from
/// its segments. `None` when there is no language, no truecolor, or the language is not covered.
fn line_colors_for(
    segs: &[crate::diff::Seg],
    lang: Option<&str>,
    theme: &Theme,
) -> Option<Vec<Color>> {
    let lang = lang?;
    let line: String = segs.iter().map(|s| s.text.as_str()).collect();
    crate::highlight::line_colors(&line, lang, theme)
}

/// Colored spans of a diff line content (US-042). Emphasized segments
/// (word-diff) keep their saturated `word` style; the others get the
/// syntax tint `colors[ci]` on the `base` background (the `+`/`-` sign and the add/
/// remove background, laid out by the caller, are never hidden). `colors = None` ->
/// everything in `base` (historical rendering). Runs of the same style are merged.
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

/// Line number gutter (faint), `lineno` right-aligned on `gw`,
/// preceded by the block indentation. `None` -> empty column.
fn gutter(lineno: Option<usize>, gw: usize, theme: &Theme) -> Span<'static> {
    let n = lineno.map(|n| n.to_string()).unwrap_or_default();
    Span::styled(format!("{INDENT}{n:>gw$} "), theme.faint())
}

/// Composes a colored diff line: when it exceeds `width`, truncates (gutter
/// at the head, hence preserved); otherwise fills the end with `bg` (color band in
/// truecolor; no visible effect in 16 colors).
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

/// Truncates a line (without background) to `width` columns, gutter kept.
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
            // Every escape family, not only CSI: an adversarial tool/model
            // output can carry OSC (window title, OSC 8 hyperlink,
            // OSC 52 clipboard) or DCS, which re-arm the terminal when we
            // only neutralize `ESC [`.
            '\x1b' => match chars.peek().copied() {
                // CSI `ESC [ ... <final 0x40..=0x7E>`.
                Some('[') => {
                    chars.next();
                    drain_csi(&mut chars);
                }
                // OSC `ESC ] ... <BEL | ST>`, and DCS/SOS/PM/APC `ESC P|X|^|_ ... ST`.
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    drain_to_st(&mut chars);
                }
                // 2-byte ESC (`ESC c` reset, `ESC ( B`, ...) or a lone ESC: we drop
                // the ESC and any intermediate byte (no sequence is ever emitted).
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // 8-bit C1 introducers: neutralized WITH their body (otherwise the
            // "31m" parameters leak as text). CSI=0x9B, OSC=0x9D, DCS/PM/APC.
            '\u{9b}' => drain_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{9e}' | '\u{9f}' => drain_to_st(&mut chars),
            '\r' => {}
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            // C0 (except \n,\t,\r), DEL (0x7F) and remaining lone 8-bit C1: removed.
            c if (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Drains a CSI sequence up to its final byte (`0x40..=0x7E`), terminator included.
fn drain_csi<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    for n in chars.by_ref() {
        if ('@'..='~').contains(&n) {
            break;
        }
    }
}

/// Drains up to the String Terminator (`ESC \`) or BEL (`\x07`), the end of an
/// OSC/DCS sequence. Consumes the terminator; also stops on a bare ESC (malformed sequence).
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

/// Last `n` non-empty lines, with light markdown and truncated, for the preview
/// of the reasoning in progress.
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
    let props = footer_props(state);
    // Codex frames the composer with blank rows; Pyxis keeps its two full-width
    // rules instead, so the input reads as a bounded field and the status line
    // sits directly under the closing rule.
    let footer_height = if state.shutdown_in_progress() {
        0
    } else {
        footer::height(&props, area.width)
    };
    // The received area can be SMALLER than the requested height (short terminal,
    // US-010 AC6): the composer takes what is left after progress and footer,
    // instead of overflowing.
    let (progress_area, composer_area, footer_area) = if progress_visible(state) {
        let rows = Layout::vertical([
            Constraint::Length(PROGRESS_HEIGHT),
            Constraint::Length(PROGRESS_GAP_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);
        (Some(rows[0]), rows[2], rows[3])
    } else {
        let rows =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).split(area);
        (None, rows[0], rows[1])
    };

    if let Some(progress_area) = progress_area {
        render_progress_line(frame, progress_area, state, theme);
    }

    // Rules only when a text line survives between them; on a crushed terminal
    // the text wins over the frame.
    let ruled = composer_area.height >= 3;
    if ruled {
        let rule = Line::from(Span::styled(
            "─".repeat(composer_area.width as usize),
            theme.composer_rule(),
        ));
        frame.render_widget(
            Paragraph::new(rule.clone()),
            Rect {
                height: 1,
                ..composer_area
            },
        );
        frame.render_widget(
            Paragraph::new(rule),
            Rect {
                y: composer_area.bottom().saturating_sub(1),
                height: 1,
                ..composer_area
            },
        );
    }

    let text_area = if ruled {
        Rect {
            y: composer_area.y + 1,
            height: composer_area.height - 2,
            ..composer_area
        }
    } else {
        composer_area
    };
    if text_area.height == 0 || text_area.width == 0 {
        return;
    }

    if state.shutdown_in_progress() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", theme.fg().add_modifier(Modifier::BOLD)),
                Span::styled("Shutting down...", theme.dim()),
            ])),
            Rect {
                height: 1,
                ..text_area
            },
        );
        return;
    }

    // Empty composer: the placeholder occupies the text row, the cursor stays
    // on the gutter. It is NOT part of `state.input`, so nothing can submit it.
    if state.input.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", theme.fg().add_modifier(Modifier::BOLD)),
                Span::styled(COMPOSER_PLACEHOLDER, theme.faint()),
            ])),
            Rect {
                height: 1,
                ..text_area
            },
        );
        frame.set_cursor_position((
            text_area
                .x
                .saturating_add(COMPOSER_GUTTER)
                .min(text_area.right().saturating_sub(1)),
            text_area.y,
        ));
        footer::render(frame, footer_area, &props, theme);
        return;
    }

    let text_width = composer_text_width(text_area.width);
    let layout = composer::layout(&state.input, state.cursor, text_width);
    let visible = text_area.height as usize;
    let offset = composer::scroll_offset(layout.cursor_row, layout.rows.len(), visible);

    let lines: Vec<Line<'static>> = layout
        .rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(idx, row)| {
            let gutter = if idx == 0 {
                Span::styled("› ", theme.fg().add_modifier(Modifier::BOLD))
            } else {
                Span::raw(INDENT)
            };
            let mut spans = vec![gutter];
            spans.extend(input_spans(
                &state.input[row.start..row.end],
                &state.skills,
                &state.files,
                theme,
                idx == 0,
            ));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), text_area);

    let cursor_row = layout.cursor_row.saturating_sub(offset).min(visible - 1) as u16;
    let col = text_area
        .x
        .saturating_add(COMPOSER_GUTTER)
        .saturating_add(layout.cursor_col as u16)
        .min(text_area.right().saturating_sub(1));
    frame.set_cursor_position((col, text_area.y + cursor_row));

    footer::render(frame, footer_area, &props, theme);
}

/// Usable width for the composer text, gutter subtracted.
fn composer_text_width(width: u16) -> usize {
    width.saturating_sub(COMPOSER_GUTTER).max(1) as usize
}

fn input_height(state: &AppState, width: u16) -> u16 {
    if state.shutdown_in_progress() {
        return SHUTDOWN_INPUT_HEIGHT;
    }
    let progress = if progress_visible(state) {
        PROGRESS_HEIGHT + PROGRESS_GAP_HEIGHT
    } else {
        0
    };
    // Height derived from the number of lines ACTUALLY rendered after folding (AC2),
    // bounded by the cap; + the 2 rules framing the text, + the footer (one
    // line, or the shortcut cheatsheet).
    let rows = composer::layout(&state.input, state.cursor, composer_text_width(width))
        .rows
        .len()
        .clamp(1, COMPOSER_MAX_ROWS as usize) as u16;
    rows + 2 + footer::height(&footer_props(state), width) + progress
}

/// Footer inputs derived from the session state.
///
/// The status line carries what Codex puts there by default: the model with its
/// reasoning level, then the workspace. The permission mode goes to the right,
/// and only when it leaves its default: an indicator that is always on says
/// nothing.
fn footer_props(state: &AppState) -> FooterProps {
    let mut status_line = Vec::new();
    let model = match state.reasoning_effort.as_deref().map(str::trim) {
        Some(effort) if !effort.is_empty() => format!("{} {effort}", state.model),
        _ => state.model.clone(),
    };
    if !model.is_empty() {
        status_line.push(StatusSegment::model(model));
    }
    if !state.workspace.is_empty() {
        status_line.push(StatusSegment::path(state.workspace.clone()));
    }
    FooterProps {
        mode: state.footer_mode(),
        status_line,
        mode_indicator: (state.permission_mode_id() != DEFAULT_PERMISSION_MODE_ID)
            .then(|| state.permission_mode_label().to_string()),
        is_task_running: matches!(state.status, Status::Thinking),
    }
}

fn progress_visible(state: &AppState) -> bool {
    matches!(state.status, Status::Thinking)
}

/// Splits an input segment into spans: every recognized `/<skill>` token gets
/// highlighted (chip), the rest in `fg`. Spaces are preserved.
///
/// `line_start` distinguishes the first visual line of the composer: only it
/// can carry a Pyxis command as its first token. Without that flag, a continuation
/// line starting with `/models` would be wrongly turned into a chip.
/// Accepted divergence: a token cut by a fold loses its highlight.
fn input_spans(
    input: &str,
    skills: &[String],
    files: &[String],
    theme: &Theme,
    line_start: bool,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, part) in input.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", theme.fg()));
        }
        if part.is_empty() {
            continue;
        }
        // Highlight: a recognized `/skill` (anywhere) OR a Pyxis command
        // as the 1st token (e.g. `/goal`, `/models`).
        let is_skill = part
            .strip_prefix('/')
            .is_some_and(|name| skills.iter().any(|s| s == name));
        let is_file = part
            .strip_prefix('@')
            .is_some_and(|path| files.iter().any(|f| f == path));
        let is_command = i == 0 && line_start && COMMANDS.iter().any(|(name, _, _)| *name == part);
        let style = if is_skill || is_file || is_command {
            theme.skill_chip()
        } else {
            theme.fg()
        };
        spans.push(Span::styled(part.to_string(), style));
    }
    spans
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

fn render_permission(frame: &mut Frame, area: Rect, prompt: &PermissionPrompt, theme: &Theme) {
    let width = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Title: crisp accent, no box. Clipped to ONE line (deterministic height).
    // Sanitized HERE (rendering point): the title carries a model-controlled
    // `path`/tool name that does NOT go through the diff engine. Without this, a `path`
    // containing OSC/CSI would inject the terminal (the diff itself is already sanitized).
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

    // Preview: SAME engine/rendering as the inline diff (US-039). Bounded to the
    // remaining room (title + actions reserved) so the answers stay ALWAYS visible.
    let actions = permission_actions(prompt, width, theme);
    let mut preview: Vec<Line<'static>> = Vec::new();
    push_diff(&mut preview, &prompt.preview, theme, width, None);
    let room = (area.height as usize).saturating_sub(1 + actions.len());
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

    lines.extend(actions);

    // Lines already clipped to the width -> no `Wrap` (exact height).
    frame.render_widget(Paragraph::new(lines), area);
}

/// Answer lines of the dialog. Single source for the rendering AND the height:
/// the two must never disagree, otherwise an option would be cut off.
///
/// US-009: the session options only appear when the answer is memoizable; when
/// it is not for a nameable reason, that reason takes their place. On a narrow
/// terminal the four options split over two lines rather than being truncated
/// (AC4).
fn permission_actions(
    prompt: &PermissionPrompt,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // (key, label) pairs; all ASCII, so their byte length is their column width.
    fn group(keys: &[(&str, &str)], theme: &Theme) -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled("  ", theme.dim())];
        for (i, (key, label)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("   ", theme.dim()));
            }
            spans.push(Span::styled(format!("[{key}]"), theme.accent()));
            spans.push(Span::styled(format!(" {label}"), theme.dim()));
        }
        spans
    }
    fn columns(spans: &[Span<'static>]) -> usize {
        spans
            .iter()
            .map(|s| measure::width(s.content.as_ref()))
            .sum()
    }

    // Labels kept short enough for the stacked form to fit 40 columns (AC4).
    const ONCE: &[(&str, &str)] = &[("o", "allow"), ("n", "deny")];
    const SESSION: &[(&str, &str)] = &[("a", "allow session"), ("d", "deny session")];

    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(note) = &prompt.memo_note {
        // The reason must stay READABLE, not merely present: on a narrow
        // terminal it wraps instead of being clipped (AC2).
        for row in wrap_words(&format!("not remembered: {}", sanitize(note)), width, 2) {
            lines.push(clip(vec![Span::styled(row, theme.faint())], width));
        }
    }
    if !prompt.memoizable {
        lines.push(clip(group(ONCE, theme), width));
        return lines;
    }
    let mut all = group(ONCE, theme);
    all.push(Span::styled("   ", theme.dim()));
    all.extend(group(SESSION, theme).into_iter().skip(1));
    if columns(&all) <= width {
        lines.push(clip(all, width));
    } else {
        // Narrow terminal (US-009 AC4): stacking keeps every option readable
        // instead of clipping one away.
        lines.push(clip(group(ONCE, theme), width));
        lines.push(clip(group(SESSION, theme), width));
    }
    lines
}

/// Wraps `text` on word boundaries into at most `max_rows` rows of `width`
/// columns, each indented by two spaces. The last row is left to the caller's
/// `clip` when the text does not fit in the budget.
fn wrap_words(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    const INDENT: &str = "  ";
    let room = width.saturating_sub(INDENT.len()).max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure::width(&candidate) <= room || current.is_empty() {
            current = candidate;
            continue;
        }
        rows.push(format!("{INDENT}{current}"));
        if rows.len() == max_rows {
            return rows;
        }
        current = word.to_string();
    }
    if !current.is_empty() {
        rows.push(format!("{INDENT}{current}"));
    }
    rows
}

/// Height needed by the permission dialog (title + bounded preview + answers).
fn permission_height(prompt: &PermissionPrompt, width: u16) -> u16 {
    // The styles play no part in the line COUNT: any theme gives the same height.
    let actions = permission_actions(prompt, width as usize, &Theme::new(false)).len() as u16;
    let preview = prompt.preview.rows.len().min(12) as u16;
    (1 + actions + preview).clamp(1 + actions, 16)
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

    /// Bottom row of the frame, where the footer lands. Scoping assertions here
    /// keeps them from matching the welcome card, which shows the same facts.
    fn footer_row(out: &str) -> &str {
        out.lines().last().unwrap_or_default()
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

    // US-019 AC1: streamed text rendered token by token (markdown), prompt present.
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

    #[test]
    /// The input is a bounded field: one full-width rule above, one below, and
    /// the status line directly under the closing rule. The input row itself
    /// keeps the terminal background (no filled block).
    fn composer_uses_rules_without_filled_background() {
        let mut s = AppState::new("gpt-5", true);
        s.set_input("Try something".into());
        let mut term = Terminal::new(TestBackend::new(48, 10)).unwrap();
        term.draw(|f| render(f, &s)).unwrap();
        let buf = term.backend().buffer();
        let prompt_y = (0..buf.area().height)
            .find(|y| (0..buf.area().width).any(|x| buf[(x, *y)].symbol() == "›"))
            .expect("composer prompt should render");
        assert!(prompt_y > 0, "composer should have a top rule");
        assert!(
            prompt_y + 1 < buf.area().height,
            "composer should have a bottom rule"
        );
        let last_x = buf.area().width.saturating_sub(1);

        for y in [prompt_y - 1, prompt_y + 1] {
            for x in [0, last_x] {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    "─",
                    "rule should span the full width at ({x}, {y})"
                );
            }
        }
        for x in 0..buf.area().width {
            assert_eq!(
                buf[(x, prompt_y)].bg,
                Color::Reset,
                "composer input row should keep the terminal background at column {x}"
            );
        }
        // The status line follows the closing rule immediately, with no blank
        // row between them.
        let footer_y = prompt_y + 2;
        assert!(
            (0..buf.area().width).any(|x| buf[(x, footer_y)].symbol() != " "),
            "the footer should sit right under the bottom rule"
        );
    }

    // Welcome screen: card with braille logo (Dyson) + identity, empty transcript.
    #[test]
    fn welcome_card_shows_logo_and_brand() {
        let mut s = AppState::new("gpt-5.5", true);
        s.workspace = "pyxis".into();
        s.provider_connected = true;
        assert!(s.is_welcome(), "empty transcript shows welcome");
        let out = draw(&s, 80, 24);
        assert!(out.contains("PYXIS"), "marque absente:\n{out}");
        // The logo is made of braille dots (U+2801..=U+28FF, blank U+2800 excluded).
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

    // The welcome screen disappears at the first message (non-empty transcript).
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

    // Terminal too narrow for the card -> compact fallback, no panic, visible mark.
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

    // Markdown is rendered, not shown raw (the `**` disappear).
    #[test]
    fn markdown_bold_is_not_shown_raw() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("This is **important** here".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 50, 10);
        assert!(out.contains("important"), "{out}");
        assert!(!out.contains("**"), "raw markdown not rendered:\n{out}");
    }

    // US-019 AC2: a diff with a gutter (line numbers) is displayed in the dialog.
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

    // Security (US-039): the dialog title (model-controlled path/tool name) is
    // sanitized at render time. A `path` carrying OSC/CSI does not leak to the terminal.
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

    // US-019 AC4: degradation without truecolor, no panic, layout intact.
    #[test]
    fn monochrome_degradation_renders_without_panic() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("mono text".into()));
        let out = draw(&s, 30, 8);
        assert!(out.contains("mono text"));
    }

    // US-019 AC4 (again): narrow terminal -> reflow without corruption (no panic).
    #[test]
    fn narrow_terminal_does_not_corrupt() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text(
            "long enough text to wrap across several lines in a narrow terminal".into(),
        ));
        let _ = draw(&s, 16, 10);
        let _ = draw(&s, 8, 6);
        // no panic = wrap indices recomputed cleanly.
    }

    // Scroll: the bound is computed on the lines AFTER wrapping, so we can
    // scroll back up to the very first turn even when the content wraps.
    #[test]
    fn scroll_up_reaches_top_of_wrapped_transcript() {
        let mut s = AppState::new("gpt-5", true);
        for i in 0..10 {
            s.push_user(format!("message number {i} with a little extra text"));
            s.apply(&AgentEvent::Text(format!("answer {i}")));
            s.apply(&AgentEvent::EndTurn);
        }
        // 1st render: publishes scroll_max (the transcript overflows the narrow window).
        let _ = draw(&s, 24, 8);
        assert!(
            s.scroll_max.get() > 0,
            "overflowing transcript should set scroll_max"
        );
        // scrolling past the bound is clamped; the 1st turn becomes visible.
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

    #[cfg(feature = "codex_tui_parity")]
    #[test]
    fn parity_transcript_overlay_shows_full_transcript_surface() {
        let mut state = AppState::new("gpt-5", true);
        state.open_transcript_overlay();
        let messages = vec![
            agent_core::Message::assistant(vec![agent_core::ContentBlock::tool_use(
                "read-1",
                "read",
                serde_json::json!({ "path": "README.md" }),
            )]),
            agent_core::Message::tool_result(
                "read-1",
                (1..=18)
                    .map(|idx| format!("{idx}\tline {idx}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                false,
            ),
        ];
        let surface = crate::history_cell::ChatSurface::from_messages(&messages);

        let bottom = draw_parity(&state, &surface, 60, 12);
        assert!(bottom.contains("T R A N S C R I P T"), "{bottom}");
        assert!(bottom.contains("ctrl+t"), "{bottom}");
        assert!(bottom.contains("line 18"), "{bottom}");
        assert!(
            !bottom.contains("›"),
            "composer should be hidden:\n{bottom}"
        );

        state.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let top = draw_parity(&state, &surface, 60, 12);
        assert!(top.contains("$ Get-Content -Raw README.md"), "{top}");
        assert!(top.contains("line 1"), "{top}");
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

    // A permission refusal interrupts cleanly (state cleaned up), AC3.
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
        assert_eq!(
            action,
            crate::state::InputAction::Permission {
                allow: false,
                remember: false
            }
        );
        assert!(s.pending.is_none());
    }

    // US-034: an assistant turn is anchored by a ● bullet.
    #[test]
    fn assistant_turn_has_bullet_anchor() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("Bonjour".into()));
        s.apply(&AgentEvent::EndTurn);
        let out = draw(&s, 40, 8);
        assert!(out.contains('●'), "puce d'ancrage absente:\n{out}");
        assert!(out.contains("Bonjour"));
    }

    // US-034: an EMPTY assistant turn leaves no orphan bullet.
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

    // US-035: an edit displays the Update(path) label + ⎿ Added/removed summary.
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
            status: None,
            structured_content: None,
            is_error: false,
            untrusted: false,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
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

    // US-035: a read displays a condensed ⎿ Read N lines summary.
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
            status: None,
            structured_content: None,
            is_error: false,
            untrusted: true,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        let out = draw(&s, 50, 10);
        assert!(out.contains("Read"), "verbe Read absent:\n{out}");
        assert!(out.contains("lines"), "line count missing:\n{out}");
    }

    // US-036: a tool error is rendered with the Error: prefix.
    #[test]
    fn tool_error_uses_error_grammar() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "x1".into(),
            content: "anchor not found in src/x.rs".into(),
            status: None,
            structured_content: None,
            is_error: true,
            untrusted: true,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        let out = draw(&s, 60, 8);
        assert!(out.contains("Error:"), "error grammar missing:\n{out}");
        assert!(out.contains("anchor not found"));
    }

    // US-036: a user rejection is distinct from an error (no "Error:").
    #[test]
    fn user_rejection_is_not_an_error() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::ToolResult(agent_core::event::ToolResultView {
            id: "x2".into(),
            content: "action \"edit\" rejected by user".into(),
            status: None,
            structured_content: None,
            is_error: true,
            untrusted: false,
            error_kind: Some(agent_core::ToolErrorKind::PermissionDenied),
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        let out = draw(&s, 64, 8);
        assert!(out.contains("rejected"), "rejection label missing:\n{out}");
        assert!(
            !out.contains("Error:"),
            "rejection should not render as an error:\n{out}"
        );
    }

    // Security (US-036 / FR-10): `sanitize` neutralizes ALL escape
    // families, not only CSI: OSC (title/hyperlink/clipboard), DCS, 8-bit C1
    // and DEL, on an adversarial tool/model output.
    #[test]
    fn sanitize_strips_all_escape_families() {
        // CSI (already covered) + OSC terminated by BEL.
        assert_eq!(sanitize("a\x1b[31mb\x1b]0;titre\x07c"), "abc");
        // OSC 8 (hyperlink) terminated by ST (ESC \).
        assert_eq!(sanitize("x\x1b]8;;http://evil\x1b\\y"), "xy");
        // DCS terminated by ST.
        assert_eq!(sanitize("p\x1bPq…data\x1b\\r"), "pr");
        // 8-bit C1 (CSI/OSC 0x9B/0x9D) and DEL removed.
        assert_eq!(sanitize("u\u{9b}31mv\u{7f}w"), "uvw");
        // Bare ESC at the end of the string: no panic, simply swallowed.
        assert_eq!(sanitize("fin\x1b"), "fin");
        // No residual ESC whatever the payload.
        let dirty = "\x1b]0;\x07\x1b[1m\u{9d}\x7f\x1bc texte";
        assert!(!sanitize(dirty).contains('\u{1b}'), "ESC residue");
    }

    // US-038: a successful edit displays the colored diff (+/- lines) under the summary.
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
            status: None,
            structured_content: None,
            is_error: false,
            untrusted: false,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        let out = draw(&s, 60, 12);
        assert!(out.contains("let x = 1;"), "removed line missing:\n{out}");
        assert!(out.contains("let x = 2;"), "added line missing:\n{out}");
        assert!(
            out.contains('+') && out.contains('-'),
            "signes de diff absents:\n{out}"
        );
    }

    // US-038: a FAILED edit displays no diff (only the error).
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
            status: None,
            structured_content: None,
            is_error: true,
            untrusted: true,
            error_kind: None,
            duration_ms: None,
            truncation: None,
            execution: None,
        }));
        let out = draw(&s, 60, 10);
        assert!(out.contains("Error:"), "error missing:\n{out}");
        assert!(
            !out.contains("YYY"),
            "no diff should render for a failed edit:\n{out}"
        );
    }

    // US-039: a very long permission diff is truncated WITHOUT hiding [y]/[n].
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

    // US-041: the cache only rebuilds the block that changes; a resize invalidates
    // everything. (The `render_rebuilds` counter instruments the previous pass.)
    #[test]
    fn cache_rebuilds_only_changed_blocks() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("Bonjour".into()));
        s.apply(&AgentEvent::EndTurn);
        s.push_user("question");
        s.apply(&AgentEvent::Text("Réponse en **gras**".into()));

        // Frame 1: cold cache -> the 3 blocks are built.
        let _ = draw(&s, 60, 20);
        assert_eq!(s.render_rebuilds(), 3, "1re frame : tout construit");

        // Frame 2: transcript unchanged -> 100% cache hit.
        let _ = draw(&s, 60, 20);
        assert_eq!(s.render_rebuilds(), 0, "blocs baked servis depuis le cache");

        // A token arrives on the last block (stream) -> a single rebuild.
        s.apply(&AgentEvent::Text(" et suite".into()));
        let _ = draw(&s, 60, 20);
        assert_eq!(
            s.render_rebuilds(),
            1,
            "seul le bloc en stream est reconstruit"
        );

        // Resize (reflow) -> cache invalidated -> everything rebuilt.
        let _ = draw(&s, 40, 20);
        assert_eq!(s.render_rebuilds(), 3, "le resize invalide tout le cache");
    }

    // US-043: a markdown table is rendered aligned in the transcript (the content
    // width is correctly passed to `render_markdown`).
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

    // US-042: diff coloring preserves the word-diff emphasis and applies the
    // syntax tint to the non-emphasized segments, without hiding the background.
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
        // Without coloring: text intact, the emphasis carries the saturated `word` style.
        let spans = diff_segs_spans(&segs, None, theme.diff_add(), theme.diff_add_word());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "let x");
        assert!(
            spans
                .iter()
                .any(|s| s.content.as_ref() == "x" && s.style == theme.diff_add_word()),
            "emphasized segment should keep word-diff style"
        );

        // With one color per character: the non-emphasized ones take the given tint
        // (fg) while keeping the `base` background; the emphasized one stays `word`.
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

    // US-042 (robustness): the color <-> character alignment of the diff holds on
    // multi-byte input (otherwise the tint drifts after the 1st accented char).
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
        // Text rebuilt intact AND every character tinted (no fallback to `base`
        // for lack of multi-byte alignment).
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, line);
        assert!(
            spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Rgb(9, 9, 9))),
            "complete tint, no misalignment on multibyte characters"
        );
    }

    // US-044/045: during a turn, a Codex-like line is displayed above the composer.
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

    // US-045: at the end of the turn, the right-hand indicators disappear.
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
    /// Idle, the footer spends its row on ambient context, never on a quit
    /// instruction: the reminder is earned by pressing Ctrl+C once.
    fn idle_footer_shows_the_status_line_not_a_quit_hint() {
        let mut s = AppState::new("gpt-5", true);
        s.workspace = "~/dev/pyxis".into();
        s.reasoning_effort = Some("high".into());
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("gpt-5 high · ~/dev/pyxis"),
            "status line missing:\n{out}"
        );
        assert!(
            !out.contains("to quit"),
            "idle footer should not advertise quit:\n{out}"
        );
    }

    /// The interrupt affordance belongs to the progress line (`esc to
    /// interrupt`), so the footer keeps showing ambient context while running.
    #[test]
    fn running_turn_keeps_the_status_line_and_interrupts_from_the_progress_line() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("hello");
        let out = draw(&s, 80, 12);
        assert!(out.contains("esc to interrupt"), "{out}");
        assert!(out.contains("gpt-5"), "status line missing:\n{out}");
    }

    #[test]
    fn footer_shows_ctrl_c_again_after_first_press() {
        let mut s = AppState::new("gpt-5", true);
        s.workspace = "~/dev/pyxis".into();
        s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("ctrl + c again to quit"),
            "ctrl+c again hint missing:\n{out}"
        );
        assert!(
            !footer_row(&out).contains("~/dev/pyxis"),
            "the reminder should evict the status line:\n{out}"
        );
    }

    /// `?` on an empty composer opens the cheatsheet; typing it into a draft
    /// inserts the character instead.
    #[test]
    fn question_mark_toggles_the_shortcut_overlay_only_when_the_composer_is_empty() {
        let mut s = AppState::new("gpt-5", true);
        s.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let out = draw(&s, 80, 24);
        assert!(s.input.is_empty(), "`?` should not reach the draft");
        assert!(out.contains("for commands"), "overlay missing:\n{out}");
        assert!(
            out.contains("to view transcript"),
            "overlay missing:\n{out}"
        );

        s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let out = draw(&s, 80, 24);
        assert!(
            !out.contains("for commands"),
            "overlay should close:\n{out}"
        );

        s.set_input("draft".into());
        s.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(s.input, "draft?");
        let out = draw(&s, 80, 24);
        assert!(
            !out.contains("for commands"),
            "a typed `?` must not open the overlay:\n{out}"
        );
    }

    /// The placeholder is a rendering affordance only: it never becomes input.
    #[test]
    fn empty_composer_shows_the_placeholder_without_filling_the_draft() {
        let s = AppState::new("gpt-5", true);
        let out = draw(&s, 80, 12);
        assert!(
            out.contains("› Ask Pyxis to do anything"),
            "placeholder missing:\n{out}"
        );
        assert!(s.input.is_empty());
    }

    /// Codex reserves the right half for the mode indicator, and only shows it
    /// when the mode leaves its default.
    #[test]
    fn permission_mode_reaches_the_right_of_the_footer_only_when_non_default() {
        let mut s = AppState::new("gpt-5", true);
        s.workspace = "~/dev/pyxis".into();
        let out = draw(&s, 80, 12);
        assert!(
            !footer_row(&out).contains("Ask for approval"),
            "the default mode should stay silent:\n{out}"
        );

        s.set_permission_mode("read-only");
        let out = draw(&s, 80, 12);
        let row = footer_row(&out);
        assert!(row.contains("Read Only"), "mode indicator missing:\n{out}");
        assert!(
            row.contains("~/dev/pyxis"),
            "indicator should share the footer row with the status line:\n{out}"
        );
        assert!(
            row.trim_end().ends_with("Read Only"),
            "indicator should be right-aligned:\n{out}"
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

    // US-046: the "new messages" pill only shows up when scrolled up AND content arrived.
    #[test]
    fn scroll_pill_only_when_scrolled_up_with_unseen() {
        let mut s = AppState::new("gpt-5", true);
        for i in 0..30 {
            s.push_user(format!("q{i}"));
            s.apply(&AgentEvent::Text(format!("answer {i}")));
            s.apply(&AgentEvent::EndTurn);
        }
        // Pinned at the bottom (scroll == 0): no pill.
        let bottom = draw(&s, 60, 10);
        assert!(
            !bottom.contains("new"),
            "no pill while pinned to bottom:\n{bottom}"
        );
        // The user scrolls up (scroll_max set by the previous draw), content arrives.
        s.scroll_up(3);
        s.apply(&AgentEvent::Text("fresh content outside the view".into()));
        let up = draw(&s, 60, 10);
        assert!(
            up.contains("new"),
            "pill expected after scroll plus content:\n{up}"
        );
    }

    // US-044 (robustness): the progress line does not panic on a narrow terminal.
    #[test]
    fn progress_status_line_survives_narrow_terminal() {
        let mut s = AppState::new("gpt-5", true);
        s.push_user("?");
        s.apply(&AgentEvent::Text("answer".into()));
        s.tick_progress(std::time::Duration::from_secs(3));
        // Width 8: the draw must complete (no panic, no corruption).
        let out = draw(&s, 8, 6);
        assert!(!out.is_empty());
    }
}
