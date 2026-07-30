use ratatui::Frame;

use super::ChatWidget;
use crate::state::AppState;

impl ChatWidget {
    pub fn render(&self, frame: &mut Frame, state: &AppState) {
        crate::render::render_parity(frame, state, &self.surface);
    }
}
