use super::ChatWidget;
use crate::app_event::PermissionTranscriptRequest;
use crate::state::{AppState, Block};
use agent_core::AgentEvent;

impl ChatWidget {
    pub fn push_user_message(&mut self, state: &AppState, text: impl Into<String>) {
        self.sync_local_blocks(state);
        let update = self.mapper.map_user_message(text);
        self.surface.apply_update(update);
        self.legacy_block_cursor = state.blocks.len();
    }

    pub fn handle_agent_event(&mut self, state: &AppState, event: &AgentEvent) {
        for update in self.mapper.map_event(event) {
            self.surface.apply_update(update);
        }
        self.legacy_block_cursor = state.blocks.len();
    }

    pub fn handle_permission_request(&mut self, request: PermissionTranscriptRequest) {
        for update in self.mapper.map_permission_request(request) {
            self.surface.apply_update(update);
        }
    }

    pub fn record_approval_decision(&mut self, allow: bool) {
        let update = self.mapper.map_approval_decision(allow);
        self.surface.apply_update(update);
    }

    /// Mirrors application-owned notices and errors that do not originate in
    /// the engine event stream, such as slash-command feedback.
    pub fn sync_local_blocks(&mut self, state: &AppState) {
        self.legacy_block_cursor = self.legacy_block_cursor.min(state.blocks.len());
        for block in &state.blocks[self.legacy_block_cursor..] {
            let update = match block {
                Block::Notice(message) => Some(self.mapper.map_notice(message.clone())),
                Block::Error(message) => Some(self.mapper.map_error(message.clone())),
                Block::User(_)
                | Block::Assistant { .. }
                | Block::Reasoning(_)
                | Block::ToolCall { .. }
                | Block::ToolResult { .. }
                | Block::Plan(_) => None,
            };
            if let Some(update) = update {
                self.surface.apply_update(update);
            }
        }
        self.legacy_block_cursor = state.blocks.len();
    }
}
