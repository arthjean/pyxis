//! UI-only transcript lifecycle contract for the Codex TUI parity migration.
//!
//! This module adapts `agent_core::AgentEvent` into stable transcript items without
//! moving rendering concerns into `agent-core`. Later parity stories can consume
//! the same contract to build active cells, finalized history cells, and bottom
//! pane decisions.

use agent_core::AgentEvent;
use agent_core::message::{Message, ToolErrorKind};
use std::collections::HashSet;

use crate::bottom_pane::BottomPane;
use crate::history_cell::ChatSurface;
use crate::insert_history::InsertHistoryMode;
use crate::state::{AppState, Block, PermissionPrompt};
use crate::terminal_viewport::{TerminalViewport, TerminalViewportState};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TranscriptItemId(String);

impl TranscriptItemId {
    pub fn local(prefix: &str, sequence: u64) -> Self {
        Self(format!("{prefix}-{sequence}"))
    }

    pub fn derived(prefix: &str, source_id: &str) -> Self {
        Self(format!("{prefix}:{source_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    ExecCommand,
    ToolCall,
    ToolResult,
    PermissionRequest,
    Notice,
    Error,
    StreamReset,
    TurnBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptItemStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptLifecycle {
    Started,
    Delta,
    Completed,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptToolErrorKind {
    UnknownTool,
    Parse,
    Validation,
    OutsideWorkspace,
    Io,
    Rejected,
    PermissionDenied,
    Timeout,
    Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptExecSource {
    Agent,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptExecStream {
    Stdout,
    Stderr,
    Combined,
}

impl From<ToolErrorKind> for TranscriptToolErrorKind {
    fn from(kind: ToolErrorKind) -> Self {
        match kind {
            ToolErrorKind::UnknownTool => Self::UnknownTool,
            ToolErrorKind::Parse => Self::Parse,
            ToolErrorKind::Validation => Self::Validation,
            ToolErrorKind::OutsideWorkspace => Self::OutsideWorkspace,
            ToolErrorKind::Io => Self::Io,
            ToolErrorKind::Rejected => Self::Rejected,
            ToolErrorKind::PermissionDenied => Self::PermissionDenied,
            ToolErrorKind::Timeout => Self::Timeout,
            ToolErrorKind::Semantic => Self::Semantic,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptPayload {
    Empty,
    Text {
        delta: String,
    },
    Reasoning {
        delta: String,
    },
    ExecCommand {
        command: String,
        source: TranscriptExecSource,
    },
    ExecOutput {
        content: String,
        is_error: bool,
        stream: TranscriptExecStream,
        untrusted: bool,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: String,
        is_error: bool,
        error_kind: Option<TranscriptToolErrorKind>,
        untrusted: bool,
    },
    Permission {
        tool: String,
        reason: String,
        taint_forced: bool,
        input_summary: String,
        mode: String,
        input: serde_json::Value,
    },
    Notice {
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptItem {
    pub id: Option<TranscriptItemId>,
    pub role: TranscriptRole,
    pub kind: TranscriptItemKind,
    pub status: TranscriptItemStatus,
    pub payload: TranscriptPayload,
}

impl TranscriptItem {
    pub fn new(
        id: Option<TranscriptItemId>,
        role: TranscriptRole,
        kind: TranscriptItemKind,
        status: TranscriptItemStatus,
        payload: TranscriptPayload,
    ) -> Self {
        Self {
            id,
            role,
            kind,
            status,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptUpdate {
    pub lifecycle: TranscriptLifecycle,
    pub item: TranscriptItem,
}

impl TranscriptUpdate {
    fn new(lifecycle: TranscriptLifecycle, item: TranscriptItem) -> Self {
        Self { lifecycle, item }
    }
}

#[derive(Debug, Default)]
pub struct TranscriptMapper {
    next_local_id: u64,
    active_assistant_id: Option<TranscriptItemId>,
    active_reasoning_id: Option<TranscriptItemId>,
    active_exec_tool_ids: HashSet<String>,
}

impl TranscriptMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map_user_message(&mut self, text: impl Into<String>) -> TranscriptUpdate {
        let id = self.next_local("user");
        TranscriptUpdate::new(
            TranscriptLifecycle::Completed,
            TranscriptItem::new(
                Some(id),
                TranscriptRole::User,
                TranscriptItemKind::UserMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Text { delta: text.into() },
            ),
        )
    }

    pub fn map_event(&mut self, event: &AgentEvent) -> Vec<TranscriptUpdate> {
        match event {
            AgentEvent::StreamReset => self.reset_active_streams(),
            AgentEvent::Text(delta) => {
                let (id, lifecycle) = self.active_assistant();
                vec![TranscriptUpdate::new(
                    lifecycle,
                    TranscriptItem::new(
                        Some(id),
                        TranscriptRole::Assistant,
                        TranscriptItemKind::AssistantMessage,
                        TranscriptItemStatus::Running,
                        TranscriptPayload::Text {
                            delta: delta.clone(),
                        },
                    ),
                )]
            }
            AgentEvent::Reasoning(delta) => {
                let (id, lifecycle) = self.active_reasoning();
                vec![TranscriptUpdate::new(
                    lifecycle,
                    TranscriptItem::new(
                        Some(id),
                        TranscriptRole::Assistant,
                        TranscriptItemKind::Reasoning,
                        TranscriptItemStatus::Running,
                        TranscriptPayload::Reasoning {
                            delta: delta.clone(),
                        },
                    ),
                )]
            }
            AgentEvent::ToolCall(view) => {
                let mut updates = self.drain_active_streams();
                if let Some(command) = bash_command(&view.name, &view.input) {
                    self.active_exec_tool_ids.insert(view.id.clone());
                    updates.push(TranscriptUpdate::new(
                        TranscriptLifecycle::Started,
                        TranscriptItem::new(
                            Some(TranscriptItemId::derived("exec", &view.id)),
                            TranscriptRole::Assistant,
                            TranscriptItemKind::ExecCommand,
                            TranscriptItemStatus::Running,
                            TranscriptPayload::ExecCommand {
                                command,
                                source: TranscriptExecSource::Agent,
                            },
                        ),
                    ));
                } else {
                    updates.push(TranscriptUpdate::new(
                        TranscriptLifecycle::Started,
                        TranscriptItem::new(
                            Some(TranscriptItemId::derived("tool", &view.id)),
                            TranscriptRole::Assistant,
                            TranscriptItemKind::ToolCall,
                            TranscriptItemStatus::Running,
                            TranscriptPayload::ToolCall {
                                name: view.name.clone(),
                                input: view.input.clone(),
                            },
                        ),
                    ));
                }
                updates
            }
            AgentEvent::ToolResult(view) => {
                let mut updates = self.drain_active_streams();
                let status = if view.is_error {
                    TranscriptItemStatus::Failed
                } else {
                    TranscriptItemStatus::Complete
                };
                if self.active_exec_tool_ids.remove(&view.id) {
                    updates.push(TranscriptUpdate::new(
                        TranscriptLifecycle::Completed,
                        TranscriptItem::new(
                            Some(TranscriptItemId::derived("exec", &view.id)),
                            TranscriptRole::Assistant,
                            TranscriptItemKind::ExecCommand,
                            status,
                            TranscriptPayload::ExecOutput {
                                content: view.content.clone(),
                                is_error: view.is_error,
                                stream: TranscriptExecStream::Combined,
                                untrusted: view.untrusted,
                            },
                        ),
                    ));
                } else {
                    updates.push(TranscriptUpdate::new(
                        TranscriptLifecycle::Completed,
                        TranscriptItem::new(
                            Some(TranscriptItemId::derived("tool", &view.id)),
                            TranscriptRole::Assistant,
                            TranscriptItemKind::ToolCall,
                            status,
                            TranscriptPayload::ToolResult {
                                content: view.content.clone(),
                                is_error: view.is_error,
                                error_kind: view.error_kind.map(TranscriptToolErrorKind::from),
                                untrusted: view.untrusted,
                            },
                        ),
                    ));
                }
                updates
            }
            AgentEvent::Compacted(kind) => vec![TranscriptUpdate::new(
                TranscriptLifecycle::Completed,
                TranscriptItem::new(
                    Some(self.next_local("notice")),
                    TranscriptRole::System,
                    TranscriptItemKind::Notice,
                    TranscriptItemStatus::Complete,
                    TranscriptPayload::Notice {
                        message: format!("compacted:{kind:?}"),
                    },
                ),
            )],
            AgentEvent::PermissionAsk(req) => {
                let mut updates = self.drain_active_streams();
                updates.push(TranscriptUpdate::new(
                    TranscriptLifecycle::Started,
                    TranscriptItem::new(
                        Some(TranscriptItemId::derived("permission", &req.call_id)),
                        TranscriptRole::System,
                        TranscriptItemKind::PermissionRequest,
                        TranscriptItemStatus::Pending,
                        TranscriptPayload::Permission {
                            tool: req.tool.clone(),
                            reason: req.reason.clone(),
                            taint_forced: req.taint_forced,
                            input_summary: req.input_summary.clone(),
                            mode: req.mode.clone(),
                            input: req.input.clone(),
                        },
                    ),
                ));
                updates
            }
            AgentEvent::EndTurn => self.complete_active_streams(),
            AgentEvent::Exhausted(reason) => {
                let mut updates = self.drain_active_streams();
                updates.push(TranscriptUpdate::new(
                    TranscriptLifecycle::Completed,
                    TranscriptItem::new(
                        Some(self.next_local("notice")),
                        TranscriptRole::System,
                        TranscriptItemKind::Notice,
                        TranscriptItemStatus::Complete,
                        TranscriptPayload::Notice {
                            message: format!("exhausted:{reason:?}"),
                        },
                    ),
                ));
                updates
            }
            AgentEvent::Error(error) => {
                let mut updates = self.drain_active_streams();
                updates.push(TranscriptUpdate::new(
                    TranscriptLifecycle::Completed,
                    TranscriptItem::new(
                        Some(self.next_local("error")),
                        TranscriptRole::System,
                        TranscriptItemKind::Error,
                        TranscriptItemStatus::Failed,
                        TranscriptPayload::Error {
                            message: error.to_string(),
                        },
                    ),
                ));
                updates
            }
        }
    }

    fn next_local(&mut self, prefix: &str) -> TranscriptItemId {
        self.next_local_id += 1;
        TranscriptItemId::local(prefix, self.next_local_id)
    }

    fn active_assistant(&mut self) -> (TranscriptItemId, TranscriptLifecycle) {
        if let Some(id) = &self.active_assistant_id {
            return (id.clone(), TranscriptLifecycle::Delta);
        }
        let id = self.next_local("assistant");
        self.active_assistant_id = Some(id.clone());
        (id, TranscriptLifecycle::Started)
    }

    fn active_reasoning(&mut self) -> (TranscriptItemId, TranscriptLifecycle) {
        if let Some(id) = &self.active_reasoning_id {
            return (id.clone(), TranscriptLifecycle::Delta);
        }
        let id = self.next_local("reasoning");
        self.active_reasoning_id = Some(id.clone());
        (id, TranscriptLifecycle::Started)
    }

    fn complete_active_streams(&mut self) -> Vec<TranscriptUpdate> {
        let mut updates = self.drain_active_streams();
        updates.push(TranscriptUpdate::new(
            TranscriptLifecycle::Completed,
            TranscriptItem::new(
                Some(self.next_local("turn")),
                TranscriptRole::System,
                TranscriptItemKind::TurnBoundary,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Empty,
            ),
        ));
        updates
    }

    fn drain_active_streams(&mut self) -> Vec<TranscriptUpdate> {
        let mut updates = Vec::new();
        if let Some(id) = self.active_reasoning_id.take() {
            updates.push(TranscriptUpdate::new(
                TranscriptLifecycle::Completed,
                TranscriptItem::new(
                    Some(id),
                    TranscriptRole::Assistant,
                    TranscriptItemKind::Reasoning,
                    TranscriptItemStatus::Complete,
                    TranscriptPayload::Empty,
                ),
            ));
        }
        if let Some(id) = self.active_assistant_id.take() {
            updates.push(TranscriptUpdate::new(
                TranscriptLifecycle::Completed,
                TranscriptItem::new(
                    Some(id),
                    TranscriptRole::Assistant,
                    TranscriptItemKind::AssistantMessage,
                    TranscriptItemStatus::Complete,
                    TranscriptPayload::Empty,
                ),
            ));
        }
        updates
    }

    fn reset_active_streams(&mut self) -> Vec<TranscriptUpdate> {
        let mut updates = Vec::new();
        if let Some(id) = self.active_reasoning_id.take() {
            updates.push(self.reset_update(id, TranscriptItemKind::Reasoning));
        }
        if let Some(id) = self.active_assistant_id.take() {
            updates.push(self.reset_update(id, TranscriptItemKind::AssistantMessage));
        }
        if updates.is_empty() {
            updates.push(TranscriptUpdate::new(
                TranscriptLifecycle::Reset,
                TranscriptItem::new(
                    Some(self.next_local("reset")),
                    TranscriptRole::System,
                    TranscriptItemKind::StreamReset,
                    TranscriptItemStatus::Cancelled,
                    TranscriptPayload::Empty,
                ),
            ));
        }
        updates
    }

    fn reset_update(&self, id: TranscriptItemId, kind: TranscriptItemKind) -> TranscriptUpdate {
        TranscriptUpdate::new(
            TranscriptLifecycle::Reset,
            TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                kind,
                TranscriptItemStatus::Cancelled,
                TranscriptPayload::Empty,
            ),
        )
    }
}

fn bash_command(name: &str, input: &serde_json::Value) -> Option<String> {
    if name != "bash" {
        return None;
    }
    input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    UserSubmitted(String),
    InputQueued(String),
    Agent(AgentEvent),
    PermissionPrompt(PermissionPrompt),
    ApprovalDecision {
        allow: bool,
    },
    Resize {
        width: u16,
        height: u16,
        active_height: u16,
    },
    CommitTick,
    HistoryInsertFailed(String),
    FatalExit(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppDispatchOutcome {
    pub agent_stopped: bool,
    pub should_exit: bool,
    pub commit_revision: u64,
}

pub struct AppEventDispatcher {
    mapper: TranscriptMapper,
    surface: ChatSurface,
    viewport: TerminalViewportState,
    bottom_pane: BottomPane,
    commit_revision: u64,
    fatal_error: Option<String>,
}

impl AppEventDispatcher {
    pub fn new(messages: &[Message], viewport: TerminalViewport, mode: InsertHistoryMode) -> Self {
        Self {
            mapper: TranscriptMapper::new(),
            surface: ChatSurface::from_messages(messages),
            viewport: TerminalViewportState::new(viewport, mode),
            bottom_pane: BottomPane::new(),
            commit_revision: 0,
            fatal_error: None,
        }
    }

    pub fn surface(&self) -> &ChatSurface {
        &self.surface
    }

    pub fn surface_mut(&mut self) -> &mut ChatSurface {
        &mut self.surface
    }

    pub fn viewport(&self) -> &TerminalViewportState {
        &self.viewport
    }

    pub fn bottom_pane(&self) -> &BottomPane {
        &self.bottom_pane
    }

    pub fn bottom_pane_mut(&mut self) -> &mut BottomPane {
        &mut self.bottom_pane
    }

    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal_error.as_deref()
    }

    pub fn dispatch(&mut self, state: &mut AppState, event: AppEvent) -> AppDispatchOutcome {
        let mut outcome = AppDispatchOutcome::default();
        match event {
            AppEvent::UserSubmitted(prompt) => {
                state.push_user(prompt.clone());
                self.surface
                    .apply_update(self.mapper.map_user_message(prompt));
            }
            AppEvent::InputQueued(prompt) => {
                state.push_user(prompt.clone());
                self.surface
                    .apply_update(self.mapper.map_user_message(prompt));
                state
                    .blocks
                    .push(Block::Notice("Message ajouté à la file d'attente.".into()));
            }
            AppEvent::Agent(event) => {
                outcome.agent_stopped = matches!(
                    event,
                    AgentEvent::EndTurn | AgentEvent::Error(_) | AgentEvent::Exhausted(_)
                );
                state.apply(&event);
                for update in self.mapper.map_event(&event) {
                    self.surface.apply_update(update);
                }
            }
            AppEvent::PermissionPrompt(prompt) => {
                state.pending = Some(prompt);
            }
            AppEvent::ApprovalDecision { allow } => {
                state.pending = None;
                let label = if allow {
                    "permission approuvée"
                } else {
                    "permission refusée"
                };
                state.blocks.push(Block::Notice(label.into()));
            }
            AppEvent::Resize {
                width,
                height,
                active_height,
            } => {
                self.viewport.resize(width, height, active_height);
            }
            AppEvent::CommitTick => {
                self.commit_revision = self.commit_revision.saturating_add(1);
            }
            AppEvent::HistoryInsertFailed(reason) => {
                self.viewport.activate_legacy_fallback(reason.clone());
                state.blocks.push(Block::Notice(format!(
                    "Terminal scrollback fallback active: {reason}"
                )));
            }
            AppEvent::FatalExit(message) => {
                self.fatal_error = Some(message.clone());
                state.blocks.push(Block::Error(message));
                state.should_quit = true;
                outcome.should_exit = true;
            }
        }
        outcome.commit_revision = self.commit_revision;
        outcome.should_exit |= state.should_quit;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{PermissionReq, ToolCallView, ToolResultView};

    #[test]
    fn text_deltas_share_a_local_id_until_end_turn() {
        let mut mapper = TranscriptMapper::new();

        let first = mapper.map_event(&AgentEvent::Text("bon".into()));
        let second = mapper.map_event(&AgentEvent::Text("jour".into()));
        let end = mapper.map_event(&AgentEvent::EndTurn);

        let first_id = first[0].item.id.as_ref().map(TranscriptItemId::as_str);
        assert_eq!(first[0].lifecycle, TranscriptLifecycle::Started);
        assert_eq!(second[0].lifecycle, TranscriptLifecycle::Delta);
        assert_eq!(
            second[0].item.id.as_ref().map(TranscriptItemId::as_str),
            first_id
        );
        assert_eq!(end[0].lifecycle, TranscriptLifecycle::Completed);
        assert_eq!(
            end[0].item.id.as_ref().map(TranscriptItemId::as_str),
            first_id
        );
        assert_eq!(end[1].item.kind, TranscriptItemKind::TurnBoundary);
    }

    #[test]
    fn stream_reset_cancels_active_local_items() {
        let mut mapper = TranscriptMapper::new();
        let text = mapper.map_event(&AgentEvent::Text("draft".into()));
        let reset = mapper.map_event(&AgentEvent::StreamReset);

        assert_eq!(reset.len(), 1);
        assert_eq!(reset[0].lifecycle, TranscriptLifecycle::Reset);
        assert_eq!(reset[0].item.status, TranscriptItemStatus::Cancelled);
        assert_eq!(reset[0].item.id, text[0].item.id);
    }

    #[test]
    fn tool_call_and_result_use_the_core_tool_id() {
        let mut mapper = TranscriptMapper::new();
        let call = mapper.map_event(&AgentEvent::ToolCall(ToolCallView {
            id: "call-1".into(),
            name: "read".into(),
            input: serde_json::json!({ "path": "Cargo.toml" }),
        }));
        let result = mapper.map_event(&AgentEvent::ToolResult(ToolResultView {
            id: "call-1".into(),
            content: "ok".into(),
            is_error: false,
            error_kind: None,
            untrusted: true,
        }));

        assert_eq!(call[0].lifecycle, TranscriptLifecycle::Started);
        assert_eq!(result[0].lifecycle, TranscriptLifecycle::Completed);
        assert_eq!(result[0].item.role, call[0].item.role);
        assert_eq!(result[0].item.kind, call[0].item.kind);
        assert_eq!(
            call[0].item.id.as_ref().map(TranscriptItemId::as_str),
            Some("tool:call-1")
        );
        assert_eq!(result[0].item.id, call[0].item.id);
    }

    #[test]
    fn bash_tool_maps_to_exec_lifecycle() {
        let mut mapper = TranscriptMapper::new();
        let call = mapper.map_event(&AgentEvent::ToolCall(ToolCallView {
            id: "call-1".into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "cargo test" }),
        }));
        let result = mapper.map_event(&AgentEvent::ToolResult(ToolResultView {
            id: "call-1".into(),
            content: "ok".into(),
            is_error: false,
            error_kind: None,
            untrusted: true,
        }));

        assert_eq!(call[0].item.kind, TranscriptItemKind::ExecCommand);
        assert_eq!(call[0].lifecycle, TranscriptLifecycle::Started);
        assert_eq!(result[0].item.kind, TranscriptItemKind::ExecCommand);
        assert_eq!(result[0].lifecycle, TranscriptLifecycle::Completed);
        assert_eq!(
            result[0].item.payload,
            TranscriptPayload::ExecOutput {
                content: "ok".into(),
                is_error: false,
                stream: TranscriptExecStream::Combined,
                untrusted: true,
            }
        );
        assert_eq!(
            result[0].item.id.as_ref().map(TranscriptItemId::as_str),
            Some("exec:call-1")
        );
    }

    #[test]
    fn tool_result_maps_core_error_kind_into_ui_error_kind() {
        let mut mapper = TranscriptMapper::new();
        let result = mapper.map_event(&AgentEvent::ToolResult(ToolResultView {
            id: "call-1".into(),
            content: "denied".into(),
            is_error: true,
            error_kind: Some(ToolErrorKind::PermissionDenied),
            untrusted: true,
        }));

        assert_eq!(result[0].item.status, TranscriptItemStatus::Failed);
        assert_eq!(
            result[0].item.payload,
            TranscriptPayload::ToolResult {
                content: "denied".into(),
                is_error: true,
                error_kind: Some(TranscriptToolErrorKind::PermissionDenied),
                untrusted: true,
            }
        );
    }

    #[test]
    fn tool_call_completes_active_stream_first() {
        let mut mapper = TranscriptMapper::new();
        let text = mapper.map_event(&AgentEvent::Text("before tool".into()));
        let updates = mapper.map_event(&AgentEvent::ToolCall(ToolCallView {
            id: "call-1".into(),
            name: "bash".into(),
            input: serde_json::json!({ "cmd": "pwd" }),
        }));

        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].lifecycle, TranscriptLifecycle::Completed);
        assert_eq!(updates[0].item.id, text[0].item.id);
        assert_eq!(updates[1].lifecycle, TranscriptLifecycle::Started);
        assert_eq!(updates[1].item.kind, TranscriptItemKind::ToolCall);
    }

    #[test]
    fn permission_request_is_pending_and_ui_scoped() {
        let mut mapper = TranscriptMapper::new();
        let update = mapper.map_event(&AgentEvent::PermissionAsk(PermissionReq {
            call_id: "call-2".into(),
            tool: "bash".into(),
            reason: "writes file".into(),
            taint_forced: false,
            input_summary: "cargo test".into(),
            input: serde_json::json!({ "cmd": "cargo test" }),
            mode: "ask".into(),
        }));

        assert_eq!(update[0].item.kind, TranscriptItemKind::PermissionRequest);
        assert_eq!(update[0].item.status, TranscriptItemStatus::Pending);
        assert_eq!(
            update[0].item.id.as_ref().map(TranscriptItemId::as_str),
            Some("permission:call-2")
        );
    }

    #[test]
    fn app_dispatcher_routes_transcript_resize_commit_and_exit() {
        let mut dispatcher = AppEventDispatcher::new(
            &[],
            TerminalViewport::new(80, 24, 8),
            InsertHistoryMode::InlineScrollback,
        );
        let mut state = AppState::new("gpt-5", false);

        dispatcher.dispatch(&mut state, AppEvent::UserSubmitted("hello".into()));
        assert_eq!(state.blocks.len(), 1);
        assert_eq!(dispatcher.surface().transcript_cells().len(), 1);

        let out = dispatcher.dispatch(&mut state, AppEvent::Agent(AgentEvent::Text("ok".into())));
        assert!(!out.agent_stopped);
        let out = dispatcher.dispatch(&mut state, AppEvent::Agent(AgentEvent::EndTurn));
        assert!(out.agent_stopped);
        assert_eq!(dispatcher.surface().transcript_cells().len(), 2);

        dispatcher.dispatch(
            &mut state,
            AppEvent::Resize {
                width: 100,
                height: 30,
                active_height: 10,
            },
        );
        assert_eq!(dispatcher.viewport().viewport().width, 100);

        let out = dispatcher.dispatch(&mut state, AppEvent::CommitTick);
        assert_eq!(out.commit_revision, 1);

        let out = dispatcher.dispatch(&mut state, AppEvent::FatalExit("boom".into()));
        assert!(out.should_exit);
        assert_eq!(dispatcher.fatal_error(), Some("boom"));
    }

    #[test]
    fn app_dispatcher_routes_permission_and_fallback() {
        let mut dispatcher = AppEventDispatcher::new(
            &[],
            TerminalViewport::new(80, 24, 8),
            InsertHistoryMode::InlineScrollback,
        );
        let mut state = AppState::new("gpt-5", false);

        dispatcher.dispatch(
            &mut state,
            AppEvent::PermissionPrompt(PermissionPrompt::new(
                "bash",
                "writes",
                crate::diff::Diff::default(),
            )),
        );
        assert!(state.pending.is_some());

        dispatcher.dispatch(&mut state, AppEvent::ApprovalDecision { allow: false });
        assert!(state.pending.is_none());
        assert!(
            state
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(t) if t.contains("refus")))
        );

        dispatcher.dispatch(&mut state, AppEvent::HistoryInsertFailed("io".into()));
        assert_eq!(dispatcher.viewport().mode(), InsertHistoryMode::Legacy);
    }
}
