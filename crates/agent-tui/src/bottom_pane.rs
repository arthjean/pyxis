//! Codex-style bottom pane contracts for the parity renderer.
//!
//! The existing Pyxis input state remains the live composer. This module adds
//! the view-stack boundary around it: transient views get first chance to handle
//! key and paste events, then the event falls back to `AppState`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::state::{AppState, InputAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCompletion {
    Pending,
    Accepted,
    Dismissed,
}

pub trait BottomPaneView {
    fn render(&self, area: Rect, buf: &mut Buffer);

    fn desired_height(&self, width: u16) -> u16;

    fn handle_key_event(&mut self, _key: KeyEvent) -> bool {
        false
    }

    fn handle_paste(&mut self, _pasted: &str) -> bool {
        false
    }

    fn completion(&self) -> ViewCompletion {
        ViewCompletion::Pending
    }

    fn dismiss_after_child_accept(&self) -> bool {
        false
    }

    fn clear_dismiss_after_child_accept(&mut self) {}

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        false
    }

    fn requires_action(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct BottomPane {
    view_stack: Vec<Box<dyn BottomPaneView>>,
}

impl BottomPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_view(&mut self, view: Box<dyn BottomPaneView>) {
        self.view_stack.push(view);
    }

    pub fn depth(&self) -> usize {
        self.view_stack.len()
    }

    pub fn active_view(&self) -> Option<&(dyn BottomPaneView + '_)> {
        self.view_stack.last().map(Box::as_ref)
    }

    pub fn active_view_mut(&mut self) -> Option<&mut (dyn BottomPaneView + '_)> {
        match self.view_stack.last_mut() {
            Some(view) => Some(view.as_mut()),
            None => None,
        }
    }

    pub fn desired_height(&self, width: u16, composer_height: u16) -> u16 {
        self.active_view()
            .map(|view| view.desired_height(width))
            .unwrap_or(composer_height)
    }

    pub fn requires_action(&self) -> bool {
        self.view_stack.iter().any(|view| view.requires_action())
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) -> bool {
        let Some(view) = self.active_view() else {
            return false;
        };
        view.render(area, buf);
        true
    }

    pub fn route_key(&mut self, state: &mut AppState, key: KeyEvent) -> InputAction {
        if self.route_view_key(key) {
            self.remove_completed_views();
            return InputAction::None;
        }
        state.on_key(key)
    }

    pub fn route_paste(&mut self, state: &mut AppState, pasted: &str) -> bool {
        if let Some(view) = self.active_view_mut()
            && view.handle_paste(pasted)
        {
            self.remove_completed_views();
            return true;
        }
        if state.pending.is_none() {
            state.insert_str(pasted);
            return true;
        }
        false
    }

    pub fn remove_completed_views(&mut self) {
        while matches!(
            self.view_stack.last().map(|view| view.completion()),
            Some(ViewCompletion::Accepted | ViewCompletion::Dismissed)
        ) {
            let completion = self
                .view_stack
                .last()
                .map(|view| view.completion())
                .unwrap_or(ViewCompletion::Pending);
            self.view_stack.pop();
            if completion == ViewCompletion::Accepted {
                while self
                    .view_stack
                    .last()
                    .is_some_and(|view| view.dismiss_after_child_accept())
                {
                    self.view_stack.pop();
                }
            } else if let Some(parent) = self.view_stack.last_mut() {
                parent.clear_dismiss_after_child_accept();
            }
        }
    }

    fn route_view_key(&mut self, key: KeyEvent) -> bool {
        let Some(view) = self.active_view_mut() else {
            return false;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return view.handle_key_event(key);
        }
        if key.code == KeyCode::Esc && !view.prefer_esc_to_handle_key_event() {
            return view.handle_key_event(key);
        }
        view.handle_key_event(key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRow {
    pub id: String,
    pub label: String,
    pub hint: String,
    pub enabled: bool,
    pub side_content: Vec<String>,
}

impl SelectionRow {
    pub fn new(id: impl Into<String>, label: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            hint: hint.into(),
            enabled: true,
            side_content: Vec::new(),
        }
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    pub fn with_side_content(mut self, lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.side_content = lines.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionTab {
    pub id: String,
    pub label: String,
}

impl SelectionTab {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSelectionView {
    title: String,
    rows: Vec<SelectionRow>,
    tabs: Vec<SelectionTab>,
    active_tab: usize,
    selected: usize,
    query: String,
    footer_hints: Vec<String>,
    completion: ViewCompletion,
    dismiss_parent_on_accept: bool,
    requires_action: bool,
}

impl ListSelectionView {
    pub fn new(title: impl Into<String>, rows: Vec<SelectionRow>) -> Self {
        let mut view = Self {
            title: title.into(),
            rows,
            tabs: Vec::new(),
            active_tab: 0,
            selected: 0,
            query: String::new(),
            footer_hints: vec!["Enter accepter".into(), "Esc annuler".into()],
            completion: ViewCompletion::Pending,
            dismiss_parent_on_accept: false,
            requires_action: false,
        };
        view.ensure_selectable();
        view
    }

    pub fn with_tabs(mut self, tabs: Vec<SelectionTab>) -> Self {
        self.tabs = tabs;
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        self
    }

    pub fn with_footer_hints(mut self, hints: Vec<String>) -> Self {
        self.footer_hints = hints;
        self
    }

    pub fn dismiss_parent_on_accept(mut self) -> Self {
        self.dismiss_parent_on_accept = true;
        self
    }

    pub fn action_required(mut self) -> Self {
        self.requires_action = true;
        self
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn active_tab_id(&self) -> Option<&str> {
        self.tabs.get(self.active_tab).map(|tab| tab.id.as_str())
    }

    pub fn selected_row(&self) -> Option<&SelectionRow> {
        self.selected_visible_enabled()
            .then(|| &self.rows[self.selected])
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                query.is_empty()
                    || row.label.to_lowercase().contains(&query)
                    || row.hint.to_lowercase().contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn ensure_selectable(&mut self) {
        if self.selected_visible_enabled() {
            return;
        }
        let visible = self.visible_indices();
        if let Some(idx) = visible.into_iter().find(|idx| self.rows[*idx].enabled) {
            self.selected = idx;
        }
    }

    fn move_selection(&mut self, step: isize) {
        let selectable = self
            .visible_indices()
            .into_iter()
            .filter(|idx| self.rows[*idx].enabled)
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            return;
        }
        let pos = selectable
            .iter()
            .position(|idx| *idx == self.selected)
            .unwrap_or(0);
        let next = (pos as isize + step).clamp(0, selectable.len() as isize - 1) as usize;
        self.selected = selectable[next];
    }

    fn accept(&mut self) {
        if self.selected_visible_enabled() {
            self.completion = ViewCompletion::Accepted;
        }
    }

    fn dismiss(&mut self) {
        self.completion = ViewCompletion::Dismissed;
    }

    fn selected_visible_enabled(&self) -> bool {
        self.visible_indices().contains(&self.selected)
            && self.rows.get(self.selected).is_some_and(|row| row.enabled)
    }
}

impl BottomPaneView for ListSelectionView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let side = self
            .rows
            .get(self.selected)
            .filter(|row| !row.side_content.is_empty());
        let columns = if area.width >= 60 && side.is_some() {
            Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(area)
        } else {
            Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(area)
        };
        Paragraph::new(self.lines(area.height)).render(columns[0], buf);
        if let Some(row) = side {
            let lines = row
                .side_content
                .iter()
                .map(|line| Line::from(Span::styled(line.clone(), Style::default())))
                .collect::<Vec<_>>();
            Paragraph::new(lines).render(columns[1], buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let visible = self.visible_indices().len().min(8) as u16;
        let tabs = u16::from(!self.tabs.is_empty());
        (1 + tabs + visible + 1).clamp(3, 12)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Tab if !self.tabs.is_empty() => {
                self.active_tab = (self.active_tab + 1) % self.tabs.len();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.ensure_selectable();
            }
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                self.query.push(c);
                self.ensure_selectable();
            }
            KeyCode::Enter => self.accept(),
            KeyCode::Esc => self.dismiss(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.dismiss(),
            _ => return false,
        }
        true
    }

    fn handle_paste(&mut self, pasted: &str) -> bool {
        self.query.push_str(pasted.trim());
        self.ensure_selectable();
        true
    }

    fn completion(&self) -> ViewCompletion {
        self.completion
    }

    fn dismiss_after_child_accept(&self) -> bool {
        self.dismiss_parent_on_accept
    }

    fn requires_action(&self) -> bool {
        self.requires_action
    }
}

impl ListSelectionView {
    fn lines(&self, max_height: u16) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(
            self.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        if !self.tabs.is_empty() {
            let tabs = self
                .tabs
                .iter()
                .enumerate()
                .map(|(idx, tab)| {
                    if idx == self.active_tab {
                        format!("[{}]", tab.label)
                    } else {
                        tab.label.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(Line::from(tabs));
        }
        let reserved = lines.len() + 1;
        let room = (max_height as usize).saturating_sub(reserved).max(1);
        for idx in self.visible_indices().into_iter().take(room) {
            let row = &self.rows[idx];
            let marker = if idx == self.selected { ">" } else { " " };
            let muted = if row.enabled { "" } else { " (disabled)" };
            lines.push(Line::from(vec![
                Span::raw(format!("{marker} {}", row.label)),
                Span::styled(format!("  {}{muted}", row.hint), Style::default()),
            ]));
        }
        lines.push(Line::from(self.footer_hints.join("  ")));
        lines
    }
}

#[cfg(test)]
mod tests;
