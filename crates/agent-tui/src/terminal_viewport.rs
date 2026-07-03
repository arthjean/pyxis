//! Inline terminal viewport geometry used by the parity renderer.
//!
//! The viewport keeps geometry separate from render state so resize handling and
//! fallback decisions are testable without a real terminal.

use crate::insert_history::InsertHistoryMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalViewport {
    pub width: u16,
    pub height: u16,
    pub active_height: u16,
}

impl TerminalViewport {
    pub fn new(width: u16, height: u16, active_height: u16) -> Self {
        let height = height.max(1);
        Self {
            width: width.max(1),
            height,
            active_height: active_height.min(height).max(1),
        }
    }

    pub fn resize(&mut self, width: u16, height: u16, active_height: u16) -> bool {
        let next = Self::new(width, height, active_height);
        let changed = *self != next;
        *self = next;
        changed
    }

    pub fn transcript_height(&self) -> u16 {
        self.height.saturating_sub(self.active_height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalViewportState {
    viewport: TerminalViewport,
    mode: InsertHistoryMode,
    revision: u64,
    fallback_notice: Option<String>,
}

impl TerminalViewportState {
    pub fn new(viewport: TerminalViewport, mode: InsertHistoryMode) -> Self {
        Self {
            viewport,
            mode,
            revision: 0,
            fallback_notice: None,
        }
    }

    pub fn viewport(&self) -> TerminalViewport {
        self.viewport
    }

    pub fn mode(&self) -> InsertHistoryMode {
        self.mode
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn fallback_notice(&self) -> Option<&str> {
        self.fallback_notice.as_deref()
    }

    pub fn resize(&mut self, width: u16, height: u16, active_height: u16) -> bool {
        let changed = self.viewport.resize(width, height, active_height);
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        changed
    }

    pub fn activate_legacy_fallback(&mut self, reason: impl Into<String>) {
        self.mode = InsertHistoryMode::Legacy;
        self.fallback_notice = Some(format!(
            "Terminal scrollback fallback active: {}",
            reason.into()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_clamps_tiny_geometry() {
        let viewport = TerminalViewport::new(0, 0, 0);

        assert_eq!(viewport.width, 1);
        assert_eq!(viewport.height, 1);
        assert_eq!(viewport.active_height, 1);
        assert_eq!(viewport.transcript_height(), 0);
    }

    #[test]
    fn resize_updates_revision_only_on_change() {
        let viewport = TerminalViewport::new(80, 24, 8);
        let mut state = TerminalViewportState::new(viewport, InsertHistoryMode::InlineScrollback);

        assert!(!state.resize(80, 24, 8));
        assert_eq!(state.revision(), 0);
        assert!(state.resize(100, 24, 10));
        assert_eq!(state.revision(), 1);
        assert_eq!(state.viewport().width, 100);
        assert_eq!(state.viewport().active_height, 10);
    }

    #[test]
    fn fallback_switches_to_legacy_with_notice() {
        let viewport = TerminalViewport::new(80, 24, 8);
        let mut state = TerminalViewportState::new(viewport, InsertHistoryMode::InlineScrollback);

        state.activate_legacy_fallback("write failed");

        assert_eq!(state.mode(), InsertHistoryMode::Legacy);
        assert_eq!(
            state.fallback_notice(),
            Some("Terminal scrollback fallback active: write failed")
        );
    }
}
