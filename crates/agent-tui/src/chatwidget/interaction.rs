use crossterm::event::KeyEvent;

use super::ChatWidget;
use crate::state::{AppState, InputAction};

impl ChatWidget {
    pub fn route_key(&mut self, state: &mut AppState, key: KeyEvent) -> InputAction {
        self.bottom_pane.route_key(state, key)
    }

    pub fn route_paste(&mut self, state: &mut AppState, pasted: &str) {
        self.bottom_pane.route_paste(state, pasted);
    }
}
