//! Main chat-surface state and orchestration.
//!
//! `ChatWidget` owns the transcript state derived from engine events and delegates
//! input routing to `BottomPane`. The agent runtime and terminal scrollback remain
//! owned by the application loop.

mod interaction;
mod rendering;
mod transcript;

use agent_core::message::Message;

use crate::app_event::TranscriptMapper;
use crate::bottom_pane::BottomPane;
use crate::history_cell::ChatSurface;

pub struct ChatWidget {
    mapper: TranscriptMapper,
    surface: ChatSurface,
    bottom_pane: BottomPane,
    legacy_block_cursor: usize,
}

impl ChatWidget {
    pub fn new(messages: &[Message]) -> Self {
        Self {
            mapper: TranscriptMapper::new(),
            surface: ChatSurface::from_messages(messages),
            bottom_pane: BottomPane::new(),
            legacy_block_cursor: crate::state::blocks_from_messages(messages).len(),
        }
    }

    pub fn surface(&self) -> &ChatSurface {
        &self.surface
    }

    pub fn surface_mut(&mut self) -> &mut ChatSurface {
        &mut self.surface
    }

    pub fn replace_messages(&mut self, messages: &[Message]) {
        self.mapper = TranscriptMapper::new();
        self.surface = ChatSurface::from_messages(messages);
        self.legacy_block_cursor = crate::state::blocks_from_messages(messages).len();
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
