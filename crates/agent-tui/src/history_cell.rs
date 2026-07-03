//! Finalized transcript cells for the Codex TUI parity path.
//!
//! `HistoryCell` is the rendering boundary for committed transcript content. It
//! stays pure: no terminal I/O, no core mutation, and no ANSI coming from
//! `agent-core`.

use std::collections::HashMap;

use agent_core::message::{ContentBlock, Message, Role, ToolCallId};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::app_event::{
    TranscriptExecSource, TranscriptExecStream, TranscriptItem, TranscriptItemId,
    TranscriptItemKind, TranscriptItemStatus, TranscriptLifecycle, TranscriptPayload,
    TranscriptRole, TranscriptUpdate,
};
use crate::insert_history::{InsertHistoryMode, PendingHistoryInsert};
use crate::measure;
use crate::render::sanitize;
use crate::streaming::StreamController;
use crate::theme::Theme;

pub const MIN_CELL_WIDTH: u16 = 1;
const MAX_TOOL_DETAIL_WIDTH: usize = 160;
const MAX_TOOL_PREVIEW_SCAN_CHARS: usize = 8192;
const MAX_LABEL_WIDTH: usize = 160;
const MAX_LABEL_CHARS: usize = 512;
const MAX_MARKDOWN_SOURCE_CHARS: usize = 65_536;
const MAX_TEXT_CELL_CHARS: usize = 65_536;
const MAX_EXEC_COMMAND_WIDTH: usize = 240;
const MAX_EXEC_OUTPUT_SCAN_CHARS: usize = 65_536;
const EXEC_OUTPUT_HEAD_LINES: usize = 2;
const EXEC_OUTPUT_TAIL_LINES: usize = 2;

pub trait HistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn desired_height(&self, width: u16) -> u16 {
        let lines = self.display_lines(width.max(MIN_CELL_WIDTH)).len().max(1);
        lines.min(u16::MAX as usize) as u16
    }
}

pub fn safe_cell_width(width: u16) -> u16 {
    width.max(MIN_CELL_WIDTH)
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryCellKind {
    User(UserCell),
    AgentMarkdown(AgentMarkdownCell),
    Reasoning(ReasoningCell),
    Notice(NoticeCell),
    Error(ErrorCell),
    Exec(ExecCell),
    Tool(ToolCell),
    FileChange(FileChangeCell),
    Composite(CompositeCell),
}

impl HistoryCellKind {
    fn append_item(&mut self, item: &TranscriptItem) {
        match self {
            Self::AgentMarkdown(cell) => {
                if let TranscriptPayload::Text { delta } = &item.payload {
                    cell.push_delta(delta);
                }
                cell.set_streaming(item.status == TranscriptItemStatus::Running);
            }
            Self::Reasoning(cell) => {
                if let TranscriptPayload::Reasoning { delta } = &item.payload {
                    append_bounded_text(&mut cell.text, delta, MAX_TEXT_CELL_CHARS);
                }
            }
            Self::Tool(cell) => cell.apply_item(item),
            Self::Exec(cell) => cell.apply_item(item),
            Self::FileChange(_) => {}
            Self::Notice(cell) => {
                if let TranscriptPayload::Notice { message } = &item.payload {
                    append_bounded_text_with_newline(
                        &mut cell.message,
                        message,
                        MAX_TEXT_CELL_CHARS,
                    );
                }
            }
            Self::Error(cell) => {
                if let TranscriptPayload::Error { message } = &item.payload {
                    append_bounded_text_with_newline(
                        &mut cell.message,
                        message,
                        MAX_TEXT_CELL_CHARS,
                    );
                }
            }
            Self::Composite(cell) => {
                if let Some(next) = cell_from_item(item) {
                    cell.cells.push(next);
                }
            }
            Self::User(_) => {}
        }
    }

    fn mark_status(&mut self, status: TranscriptItemStatus) {
        match self {
            Self::AgentMarkdown(cell) => {
                cell.set_streaming(status == TranscriptItemStatus::Running)
            }
            Self::Exec(cell) => cell.mark_status(status),
            Self::Tool(cell) => cell.status = status,
            _ => {}
        }
    }

    pub fn is_empty_control(&self) -> bool {
        matches!(self, Self::Composite(cell) if cell.cells.is_empty())
    }
}

impl HistoryCell for HistoryCellKind {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::User(cell) => cell.display_lines(width),
            Self::AgentMarkdown(cell) => cell.display_lines(width),
            Self::Reasoning(cell) => cell.display_lines(width),
            Self::Notice(cell) => cell.display_lines(width),
            Self::Error(cell) => cell.display_lines(width),
            Self::Exec(cell) => cell.display_lines(width),
            Self::Tool(cell) => cell.display_lines(width),
            Self::FileChange(cell) => cell.display_lines(width),
            Self::Composite(cell) => cell.display_lines(width),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCell {
    pub text: String,
}

impl UserCell {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: bounded_text(text.into(), MAX_TEXT_CELL_CHARS),
        }
    }
}

impl HistoryCell for UserCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let lines = text_lines(&self.text, Style::default().add_modifier(Modifier::BOLD));
        render_prefixed(
            &lines,
            Span::styled("› ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("  "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMarkdownCell {
    pub markdown: String,
    pub streaming: bool,
    controller: StreamController,
}

impl AgentMarkdownCell {
    pub fn new(markdown: impl Into<String>, streaming: bool) -> Self {
        let mut controller = StreamController::new();
        controller.push_delta(&markdown.into());
        if !streaming {
            controller.finalize();
        }
        let markdown = controller.raw_source().to_string();
        Self {
            markdown,
            streaming,
            controller,
        }
    }

    pub fn stream_view(&self) -> crate::streaming::StreamView<'_> {
        self.controller.view()
    }

    fn push_delta(&mut self, delta: &str) {
        self.controller.push_delta(delta);
        self.markdown = self.controller.raw_source().to_string();
    }

    fn set_streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
        if streaming {
            return;
        }
        self.controller.finalize();
        self.markdown = self.controller.raw_source().to_string();
    }
}

impl HistoryCell for AgentMarkdownCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = Theme::new(true);
        let content_width = safe_width_usize(width).saturating_sub(2).max(1);
        let lines = if self.streaming {
            let view = self.controller.view();
            render_streaming_markdown(view.stable_prefix, view.mutable_tail, &theme, content_width)
        } else {
            let clean = bounded_sanitize(&self.markdown, MAX_MARKDOWN_SOURCE_CHARS);
            crate::markdown::render_markdown(&clean, &theme, content_width)
        };
        render_prefixed(
            &lines,
            Span::styled("● ", theme.accent()),
            Span::raw("  "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningCell {
    pub text: String,
}

impl ReasoningCell {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: bounded_text(text.into(), MAX_TEXT_CELL_CHARS),
        }
    }
}

impl HistoryCell for ReasoningCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(
            "reasoning",
            Style::default().add_modifier(Modifier::ITALIC | Modifier::DIM),
        ))];
        if !self.text.trim().is_empty() {
            lines.extend(text_lines(
                &self.text,
                Style::default().add_modifier(Modifier::ITALIC | Modifier::DIM),
            ));
        }
        render_prefixed(
            &lines,
            Span::styled("· ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("  "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeCell {
    pub message: String,
}

impl NoticeCell {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: bounded_text(message.into(), MAX_TEXT_CELL_CHARS),
        }
    }
}

impl HistoryCell for NoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let lines = text_lines(&self.message, Style::default().add_modifier(Modifier::DIM));
        render_prefixed(
            &lines,
            Span::styled("· ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("  "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorCell {
    pub message: String,
}

impl ErrorCell {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: bounded_text(message.into(), MAX_TEXT_CELL_CHARS),
        }
    }
}

impl HistoryCell for ErrorCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let lines = text_lines(&self.message, Style::default().add_modifier(Modifier::BOLD));
        render_prefixed(
            &lines,
            Span::styled("✗ ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCell {
    calls: Vec<ExecCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecCall {
    id: Option<TranscriptItemId>,
    command: String,
    source: TranscriptExecSource,
    status: TranscriptItemStatus,
    output: String,
    is_error: bool,
    stream: TranscriptExecStream,
    kind: ExecCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecCommandKind {
    Run(String),
    Read(String),
    List(String),
    Search(String),
}

impl ExecCell {
    fn command_with_id(
        id: Option<TranscriptItemId>,
        command: &str,
        source: TranscriptExecSource,
        status: TranscriptItemStatus,
    ) -> Self {
        Self {
            calls: vec![ExecCall::new(id, command, source, status)],
        }
    }

    fn orphan(content: &str, is_error: bool, status: TranscriptItemStatus) -> Self {
        let mut call = ExecCall::new(
            None,
            "(unknown command)",
            TranscriptExecSource::Agent,
            status,
        );
        call.is_error = is_error;
        call.output = bound_exec_output_buffer(content);
        Self { calls: vec![call] }
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        match &item.payload {
            TranscriptPayload::ExecCommand { command, source } => {
                let call = ExecCall::new(item.id.clone(), command, *source, item.status);
                if let Some(existing) = self.call_mut(item.id.as_ref()) {
                    existing.command = call.command;
                    existing.source = call.source;
                    existing.status = call.status;
                    existing.kind = call.kind;
                } else if self.can_group_with(&call) {
                    self.calls.push(call);
                }
            }
            TranscriptPayload::ExecOutput {
                content,
                is_error,
                stream,
                ..
            } => {
                if let Some(existing) = self.call_mut(item.id.as_ref()) {
                    existing.append_output(content, *is_error, *stream, item.status);
                } else {
                    let mut orphan = ExecCall::new(
                        item.id.clone(),
                        "(unknown command)",
                        TranscriptExecSource::Agent,
                        item.status,
                    );
                    orphan.append_output(content, *is_error, *stream, item.status);
                    self.calls.push(orphan);
                }
            }
            _ => {}
        }
    }

    fn mark_status(&mut self, status: TranscriptItemStatus) {
        for call in &mut self.calls {
            call.status = status;
        }
    }

    fn contains_id(&self, id: Option<&TranscriptItemId>) -> bool {
        id.is_some_and(|id| self.calls.iter().any(|call| call.id.as_ref() == Some(id)))
    }

    fn can_group_item(&self, item: &TranscriptItem) -> bool {
        let TranscriptPayload::ExecCommand { command, source } = &item.payload else {
            return false;
        };
        let call = ExecCall::new(item.id.clone(), command, *source, item.status);
        self.can_group_with(&call)
    }

    fn can_group_with(&self, call: &ExecCall) -> bool {
        self.is_exploring() && call.is_exploring()
    }

    fn is_exploring(&self) -> bool {
        !self.calls.is_empty()
            && self.calls.iter().all(ExecCall::is_exploring)
            && !self.calls.iter().any(ExecCall::failed)
    }

    fn should_defer_finalization(&self) -> bool {
        self.is_exploring() && self.calls.iter().all(|call| !call.is_running())
    }

    fn call_mut(&mut self, id: Option<&TranscriptItemId>) -> Option<&mut ExecCall> {
        let id = id?;
        self.calls
            .iter_mut()
            .rev()
            .find(|call| call.id.as_ref() == Some(id))
    }

    fn any_running(&self) -> bool {
        self.calls.iter().any(ExecCall::is_running)
    }

    fn command_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (idx, call) in self.calls.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            out.extend(call.command_display_lines(width));
        }
        out
    }

    fn exploring_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let title = if self.any_running() {
            "Exploring"
        } else {
            "Explored"
        };
        let mut lines = vec![Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        lines.extend(self.exploring_detail_lines());
        render_prefixed(
            &lines,
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        )
    }

    fn exploring_detail_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut idx = 0;
        while idx < self.calls.len() {
            if matches!(self.calls[idx].kind, ExecCommandKind::Read(_)) {
                let mut names = Vec::new();
                while idx < self.calls.len() {
                    let ExecCommandKind::Read(name) = &self.calls[idx].kind else {
                        break;
                    };
                    if !names.iter().any(|known| known == name) {
                        names.push(name.clone());
                    }
                    idx += 1;
                }
                lines.push(operation_line("Read", &names.join(", ")));
                continue;
            }

            match &self.calls[idx].kind {
                ExecCommandKind::List(target) => lines.push(operation_line("List", target)),
                ExecCommandKind::Search(target) => lines.push(operation_line("Search", target)),
                ExecCommandKind::Run(command) => lines.push(operation_line("Run", command)),
                ExecCommandKind::Read(_) => {}
            }
            idx += 1;
        }
        lines
    }
}

impl HistoryCell for ExecCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_exploring() {
            self.exploring_display_lines(width)
        } else {
            self.command_display_lines(width)
        }
    }
}

impl ExecCall {
    fn new(
        id: Option<TranscriptItemId>,
        command: &str,
        source: TranscriptExecSource,
        status: TranscriptItemStatus,
    ) -> Self {
        let command = clean_exec_command(command);
        let kind = classify_exec_command(&command);
        Self {
            id,
            command,
            source,
            status,
            output: String::new(),
            is_error: false,
            stream: TranscriptExecStream::Combined,
            kind,
        }
    }

    fn is_running(&self) -> bool {
        matches!(
            self.status,
            TranscriptItemStatus::Pending | TranscriptItemStatus::Running
        )
    }

    fn failed(&self) -> bool {
        self.is_error || self.status == TranscriptItemStatus::Failed
    }

    fn is_exploring(&self) -> bool {
        !matches!(self.kind, ExecCommandKind::Run(_))
    }

    fn append_output(
        &mut self,
        content: &str,
        is_error: bool,
        stream: TranscriptExecStream,
        status: TranscriptItemStatus,
    ) {
        self.is_error = self.is_error || is_error;
        self.stream = stream;
        self.status = status;
        let clean = bounded_exec_text(content);
        if clean.is_empty() {
            return;
        }
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.output.push_str(&clean);
        self.output = bound_exec_output_buffer(&self.output);
    }

    fn command_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let title = match (self.status, self.source) {
            (TranscriptItemStatus::Pending | TranscriptItemStatus::Running, _) => "Running",
            (_, TranscriptExecSource::User) => "You ran",
            _ => "Ran",
        };
        let bullet_style = if self.is_error || self.status == TranscriptItemStatus::Failed {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let command_lines = command_logical_lines(title, &self.command);
        let mut out = render_prefixed(
            &command_lines,
            Span::styled("• ", bullet_style),
            Span::styled("  │ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        );
        out.extend(self.output_display_lines(width));
        out
    }

    fn output_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.output.trim().is_empty() {
            if self.is_running() {
                return Vec::new();
            }
            return render_prefixed(
                &[Line::from(Span::styled(
                    "(no output)",
                    Style::default().add_modifier(Modifier::DIM),
                ))],
                Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw("    "),
                width,
            );
        }

        let lines = bounded_output_lines(&self.output)
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    line,
                    Style::default().add_modifier(Modifier::DIM),
                ))
            })
            .collect::<Vec<_>>();
        render_prefixed(
            &lines,
            Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("    "),
            width,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCell {
    pub title: String,
    pub detail: String,
    pub status: TranscriptItemStatus,
    name: Option<String>,
    input: Option<Value>,
    file_change: Option<FileChangeCell>,
}

impl ToolCell {
    fn calling(name: &str, input: &Value, status: TranscriptItemStatus) -> Self {
        let label = crate::tool::label(name, input);
        let title = match label.target {
            Some(target) => format!("{}({target})", label.verb),
            None => label.verb,
        };
        Self {
            title: sanitize_label(&title),
            detail: "Calling".into(),
            status,
            name: Some(name.to_string()),
            input: Some(input.clone()),
            file_change: None,
        }
    }

    fn result(content: &str, is_error: bool, status: TranscriptItemStatus) -> Self {
        let title = if is_error {
            "Tool failed"
        } else {
            "Tool result"
        };
        Self {
            title: title.into(),
            detail: first_non_empty_line(content),
            status,
            name: None,
            input: None,
            file_change: None,
        }
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        self.status = item.status;
        match &item.payload {
            TranscriptPayload::ToolResult {
                content, is_error, ..
            } => {
                self.detail = first_non_empty_line(content);
                if *is_error {
                    self.title = "Tool failed".into();
                }
                self.file_change =
                    self.name
                        .as_deref()
                        .zip(self.input.as_ref())
                        .and_then(|(name, input)| {
                            FileChangeCell::from_tool(name, input, content, *is_error)
                        });
            }
            TranscriptPayload::ToolCall { name, input } => {
                let next = Self::calling(name, input, item.status);
                self.title = next.title;
                self.detail = next.detail;
                self.name = next.name;
                self.input = next.input;
                self.file_change = next.file_change;
            }
            _ => {}
        }
    }
}

impl HistoryCell for ToolCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let status = match self.status {
            TranscriptItemStatus::Pending => "Pending",
            TranscriptItemStatus::Running => "Calling",
            TranscriptItemStatus::Complete => "Called",
            TranscriptItemStatus::Failed => "Failed",
            TranscriptItemStatus::Cancelled => "Cancelled",
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                self.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {status}"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])];
        if !self.detail.is_empty() {
            lines.extend(text_lines(
                &self.detail,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        let mut out = render_prefixed(
            &lines,
            Span::styled("● ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("⎿ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        );
        if let Some(file_change) = &self.file_change {
            out.extend(file_change.display_lines(width));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeCell {
    pub title: String,
    pub summary: String,
    pub diff: Option<crate::diff::Diff>,
    pub failed: bool,
}

impl FileChangeCell {
    fn from_tool(name: &str, input: &Value, content: &str, is_error: bool) -> Option<Self> {
        if !matches!(name, "edit" | "write") {
            return None;
        }
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .map(sanitize_label)
            .unwrap_or_else(|| "(unknown file)".to_string());
        if is_error {
            return Some(Self {
                title: format!("File change failed {path}"),
                summary: first_non_empty_line(content),
                diff: None,
                failed: true,
            });
        }

        let action = if name == "write" { "Added" } else { "Edited" };
        let diff = crate::diff::from_tool(name, input);
        let summary = diff
            .as_ref()
            .map(diff_summary)
            .unwrap_or_else(|| "No visible diff".to_string());
        Some(Self {
            title: format!("{action} {path}"),
            summary,
            diff,
            failed: false,
        })
    }
}

impl HistoryCell for FileChangeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = Theme::new(true);
        let mut lines = vec![Line::from(vec![
            Span::styled("  ", theme.faint()),
            Span::styled(
                self.title.clone(),
                if self.failed {
                    theme.error()
                } else {
                    theme.fg().add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(format!(" {}", self.summary), theme.faint()),
        ])];

        if let Some(diff) = &self.diff {
            lines.extend(render_file_diff(diff, &theme, width));
        }
        lines
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompositeCell {
    pub cells: Vec<HistoryCellKind>,
}

impl CompositeCell {
    pub fn new(cells: Vec<HistoryCellKind>) -> Self {
        Self { cells }
    }
}

impl HistoryCell for CompositeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (idx, cell) in self.cells.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            out.extend(cell.display_lines(width));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveHistoryCell {
    pub id: Option<TranscriptItemId>,
    pub cell: HistoryCellKind,
    pub revision: u64,
}

impl ActiveHistoryCell {
    fn new(item: &TranscriptItem, cell: HistoryCellKind) -> Self {
        Self {
            id: item.id.clone(),
            cell,
            revision: 1,
        }
    }

    fn matches(&self, id: Option<&TranscriptItemId>) -> bool {
        if let HistoryCellKind::Exec(cell) = &self.cell
            && cell.contains_id(id)
        {
            return true;
        }
        self.id.as_ref() == id
    }

    fn append(&mut self, item: &TranscriptItem) {
        self.cell.append_item(item);
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChatSurface {
    transcript_cells: Vec<HistoryCellKind>,
    active_cell: Option<ActiveHistoryCell>,
    active_tools: Vec<ActiveHistoryCell>,
    pending_insert_cells: Vec<HistoryCellKind>,
}

impl ChatSurface {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: &[Message]) -> Self {
        let transcript_cells = cells_from_messages(messages);
        Self {
            pending_insert_cells: transcript_cells.clone(),
            transcript_cells,
            active_cell: None,
            active_tools: Vec::new(),
        }
    }

    pub fn transcript_cells(&self) -> &[HistoryCellKind] {
        &self.transcript_cells
    }

    pub fn active_cell(&self) -> Option<&ActiveHistoryCell> {
        self.active_cell
            .as_ref()
            .or_else(|| self.active_tools.last())
    }

    pub fn active_revision(&self) -> Option<u64> {
        self.active_cell().map(|cell| cell.revision)
    }

    pub fn apply_update(&mut self, update: TranscriptUpdate) {
        match update.lifecycle {
            TranscriptLifecycle::Started => self.start(update.item),
            TranscriptLifecycle::Delta => self.delta(update.item),
            TranscriptLifecycle::Completed => self.complete(update.item),
            TranscriptLifecycle::Reset => self.reset(update.item.id.as_ref()),
        }
    }

    pub fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.transcript_cells {
            lines.extend(cell.display_lines(width));
        }
        for active in &self.active_tools {
            lines.extend(active.cell.display_lines(width));
        }
        if let Some(active) = &self.active_cell {
            lines.extend(active.cell.display_lines(width));
        }
        lines
    }

    pub fn active_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for active in &self.active_tools {
            lines.extend(active.cell.display_lines(width));
        }
        if let Some(active) = &self.active_cell {
            lines.extend(active.cell.display_lines(width));
        }
        lines
    }

    pub fn drain_pending_insert(
        &mut self,
        width: u16,
        mode: InsertHistoryMode,
    ) -> Option<PendingHistoryInsert> {
        if self.pending_insert_cells.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for cell in self.pending_insert_cells.drain(..) {
            lines.extend(cell.display_lines(width).into_iter().map(plain_line));
        }
        match mode {
            InsertHistoryMode::Legacy => Some(PendingHistoryInsert::legacy(lines)),
            InsertHistoryMode::InlineScrollback => {
                Some(PendingHistoryInsert::inline_scrollback(lines))
            }
        }
    }

    fn start(&mut self, item: TranscriptItem) {
        if is_exec_item(&item) {
            self.start_exec(item);
            return;
        }
        if is_tool_item(&item) {
            self.finalize_deferred_execs();
            self.start_tool(item);
            return;
        }
        self.finalize_deferred_execs();
        if self
            .active_cell
            .as_ref()
            .is_some_and(|active| active.matches(item.id.as_ref()))
        {
            if let Some(active) = self.active_cell.as_mut() {
                active.append(&item);
            }
            return;
        }
        self.finalize_active();
        if let Some(cell) = cell_from_item(&item) {
            self.active_cell = Some(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn delta(&mut self, item: TranscriptItem) {
        if is_exec_item(&item) {
            self.delta_exec(item);
            return;
        }
        if is_tool_item(&item) {
            self.delta_tool(item);
            return;
        }
        if let Some(active) = self.active_cell.as_mut()
            && active.matches(item.id.as_ref())
        {
            active.append(&item);
            return;
        }
        if let Some(cell) = cell_from_item(&item) {
            self.finalize_active();
            self.active_cell = Some(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn complete(&mut self, item: TranscriptItem) {
        if is_exec_item(&item) {
            self.complete_exec(item);
            return;
        }
        if item.kind == TranscriptItemKind::TurnBoundary {
            self.finalize_deferred_execs();
        }
        if is_tool_item(&item) {
            self.complete_tool(item);
            return;
        }
        if self
            .active_cell
            .as_ref()
            .is_some_and(|active| active.matches(item.id.as_ref()))
        {
            if let Some(mut active) = self.active_cell.take() {
                active.append(&item);
                active.cell.mark_status(item.status);
                self.push_finalized(active.cell);
            }
            return;
        }
        if let Some(mut cell) = cell_from_item(&item) {
            cell.mark_status(item.status);
            self.push_finalized(cell);
        }
    }

    fn reset(&mut self, id: Option<&TranscriptItemId>) {
        if id.is_none() {
            self.active_cell = None;
            self.active_tools.clear();
            return;
        }
        if self
            .active_cell
            .as_ref()
            .is_some_and(|active| active.matches(id))
        {
            self.active_cell = None;
        }
        self.active_tools.retain(|active| !active.matches(id));
    }

    fn start_exec(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            self.active_tools[index].append(&item);
            return;
        }
        if let Some(index) = self.active_exec_group_index(&item) {
            self.active_tools[index].append(&item);
            return;
        }
        self.finalize_deferred_execs();
        self.finalize_active();
        if let Some(cell) = cell_from_item(&item) {
            self.active_tools.push(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn delta_exec(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            self.active_tools[index].append(&item);
            return;
        }
        if let Some(cell) = cell_from_item(&item) {
            self.active_tools.push(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn complete_exec(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            let mut active = self.active_tools.remove(index);
            active.append(&item);
            active.cell.mark_status(item.status);
            if active_exec_should_defer(&active.cell) {
                self.active_tools.insert(index, active);
            } else {
                self.push_finalized(active.cell);
            }
            return;
        }
        if let Some(mut cell) = cell_from_item(&item) {
            cell.mark_status(item.status);
            self.push_finalized(cell);
        }
    }

    fn start_tool(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            self.active_tools[index].append(&item);
            return;
        }
        self.finalize_active();
        if let Some(cell) = cell_from_item(&item) {
            self.active_tools.push(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn delta_tool(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            self.active_tools[index].append(&item);
            return;
        }
        if let Some(cell) = cell_from_item(&item) {
            self.active_tools.push(ActiveHistoryCell::new(&item, cell));
        }
    }

    fn complete_tool(&mut self, item: TranscriptItem) {
        if let Some(index) = self.active_tool_index(item.id.as_ref()) {
            let mut active = self.active_tools.remove(index);
            active.append(&item);
            active.cell.mark_status(item.status);
            self.push_finalized(active.cell);
            return;
        }
        if let Some(mut cell) = cell_from_item(&item) {
            cell.mark_status(item.status);
            self.push_finalized(cell);
        }
    }

    fn active_tool_index(&self, id: Option<&TranscriptItemId>) -> Option<usize> {
        self.active_tools
            .iter()
            .position(|active| active.matches(id))
    }

    fn finalize_active(&mut self) {
        if let Some(mut active) = self.active_cell.take() {
            active.cell.mark_status(TranscriptItemStatus::Complete);
            self.push_finalized(active.cell);
        }
    }

    fn finalize_deferred_execs(&mut self) {
        let mut idx = 0;
        while idx < self.active_tools.len() {
            if active_exec_should_defer(&self.active_tools[idx].cell) {
                let active = self.active_tools.remove(idx);
                self.push_finalized(active.cell);
            } else {
                idx += 1;
            }
        }
    }

    fn active_exec_group_index(&self, item: &TranscriptItem) -> Option<usize> {
        self.active_tools.iter().position(|active| {
            let HistoryCellKind::Exec(cell) = &active.cell else {
                return false;
            };
            cell.can_group_item(item)
        })
    }

    fn push_finalized(&mut self, cell: HistoryCellKind) {
        if cell.is_empty_control() {
            return;
        }
        self.pending_insert_cells.push(cell.clone());
        self.transcript_cells.push(cell);
    }
}

pub fn cells_from_messages(messages: &[Message]) -> Vec<HistoryCellKind> {
    let mut cells = Vec::new();
    let mut pending_exec_calls = HashMap::<ToolCallId, usize>::new();
    for message in messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.trim().is_empty() {
                    cells.push(HistoryCellKind::Notice(NoticeCell::new(text)));
                }
            }
            Role::User => {
                let text = message.text();
                if !text.trim().is_empty() {
                    cells.push(HistoryCellKind::User(UserCell::new(text)));
                }
                cells.extend(image_notices(message));
            }
            Role::Assistant => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.trim().is_empty() {
                                cells.push(HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new(
                                    text.clone(),
                                    false,
                                )));
                            }
                        }
                        ContentBlock::Thinking { text } => {
                            cells
                                .push(HistoryCellKind::Reasoning(ReasoningCell::new(text.clone())));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            if let Some(command) = exec_command_from_tool(name, input) {
                                cells.push(HistoryCellKind::Exec(ExecCell::command_with_id(
                                    Some(TranscriptItemId::derived("exec", id)),
                                    &command,
                                    TranscriptExecSource::Agent,
                                    TranscriptItemStatus::Complete,
                                )));
                                let index = cells.len().saturating_sub(1);
                                pending_exec_calls.insert(id.clone(), index);
                            } else {
                                cells.push(HistoryCellKind::Tool(ToolCell::calling(
                                    name,
                                    input,
                                    TranscriptItemStatus::Complete,
                                )));
                            }
                        }
                        ContentBlock::Image { media_type, .. } => {
                            cells.push(HistoryCellKind::Notice(NoticeCell::new(format!(
                                "unsupported image content: {media_type}"
                            ))));
                        }
                        ContentBlock::EncryptedReasoning { .. } => {}
                        ContentBlock::Summary { text, .. } => {
                            cells.push(HistoryCellKind::Notice(NoticeCell::new(text.clone())));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                    {
                        let status = if *is_error {
                            TranscriptItemStatus::Failed
                        } else {
                            TranscriptItemStatus::Complete
                        };
                        if let Some(index) = pending_exec_calls.get(tool_use_id).copied()
                            && let Some(HistoryCellKind::Exec(cell)) = cells.get_mut(index)
                        {
                            let item = TranscriptItem::new(
                                Some(TranscriptItemId::derived("exec", tool_use_id)),
                                TranscriptRole::Assistant,
                                TranscriptItemKind::ExecCommand,
                                status,
                                TranscriptPayload::ExecOutput {
                                    content: content.clone(),
                                    is_error: *is_error,
                                    stream: TranscriptExecStream::Combined,
                                    untrusted: true,
                                },
                            );
                            cell.apply_item(&item);
                            cell.mark_status(status);
                        } else {
                            cells.push(HistoryCellKind::Tool(ToolCell::result(
                                content, *is_error, status,
                            )));
                        }
                    }
                }
            }
        }
    }
    cells
}

fn image_notices(message: &Message) -> Vec<HistoryCellKind> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { media_type, .. } => Some(HistoryCellKind::Notice(
                NoticeCell::new(format!("unsupported image content: {media_type}")),
            )),
            _ => None,
        })
        .collect()
}

fn cell_from_item(item: &TranscriptItem) -> Option<HistoryCellKind> {
    if item.kind == TranscriptItemKind::TurnBoundary {
        return None;
    }
    match &item.payload {
        TranscriptPayload::Empty => match item.kind {
            TranscriptItemKind::StreamReset => {
                Some(HistoryCellKind::Notice(NoticeCell::new("stream reset")))
            }
            _ => None,
        },
        TranscriptPayload::Text { delta } => match item.kind {
            TranscriptItemKind::UserMessage => Some(HistoryCellKind::User(UserCell::new(delta))),
            TranscriptItemKind::AssistantMessage => Some(HistoryCellKind::AgentMarkdown(
                AgentMarkdownCell::new(delta, item.status == TranscriptItemStatus::Running),
            )),
            _ => Some(HistoryCellKind::Notice(NoticeCell::new(delta))),
        },
        TranscriptPayload::Reasoning { delta } => Some(HistoryCellKind::Reasoning(
            ReasoningCell::new(delta.clone()),
        )),
        TranscriptPayload::ExecCommand { command, source } => Some(HistoryCellKind::Exec(
            ExecCell::command_with_id(item.id.clone(), command, *source, item.status),
        )),
        TranscriptPayload::ExecOutput {
            content, is_error, ..
        } => Some(HistoryCellKind::Exec(ExecCell::orphan(
            content,
            *is_error,
            item.status,
        ))),
        TranscriptPayload::ToolCall { name, input } => Some(HistoryCellKind::Tool(
            ToolCell::calling(name, input, item.status),
        )),
        TranscriptPayload::ToolResult {
            content, is_error, ..
        } => Some(HistoryCellKind::Tool(ToolCell::result(
            content,
            *is_error,
            item.status,
        ))),
        TranscriptPayload::Permission { tool, reason, .. } => Some(HistoryCellKind::Notice(
            NoticeCell::new(format!("permission: {tool} ({reason})")),
        )),
        TranscriptPayload::Notice { message } => {
            Some(HistoryCellKind::Notice(NoticeCell::new(message)))
        }
        TranscriptPayload::Error { message } => {
            Some(HistoryCellKind::Error(ErrorCell::new(message)))
        }
    }
}

fn text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    let clean = sanitize(text);
    if clean.is_empty() {
        return vec![Line::default()];
    }
    clean
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect()
}

fn render_prefixed(
    logical_lines: &[Line<'static>],
    first_prefix: Span<'static>,
    continuation_prefix: Span<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let logical_lines = if logical_lines.is_empty() {
        vec![Line::default()]
    } else {
        logical_lines.to_vec()
    };
    let prefix_width = measure::width(first_prefix.content.as_ref())
        .max(measure::width(continuation_prefix.content.as_ref()));
    let content_width = safe_width_usize(width).saturating_sub(prefix_width).max(1);
    let mut out = Vec::new();
    let mut first = true;
    for line in logical_lines {
        for wrapped in wrap_spans(&line.spans, content_width) {
            let prefix = if first {
                first_prefix.clone()
            } else {
                continuation_prefix.clone()
            };
            first = false;
            let mut spans = vec![prefix];
            spans.extend(wrapped);
            out.push(Line::from(spans));
        }
    }
    out
}

fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut units = Vec::new();
    for span in spans {
        for grapheme in span.content.as_ref().graphemes(true) {
            units.push((grapheme.to_string(), span.style, measure::width(grapheme)));
        }
    }
    if units.is_empty() {
        return vec![Vec::new()];
    }

    let mut out = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0usize;
    let mut last_space = None;
    for unit in units {
        line_width = line_width.saturating_add(unit.2);
        line.push(unit);
        if line.last().is_some_and(|(text, _, _)| text == " ") {
            last_space = Some(line.len() - 1);
        }
        if line_width > width {
            if let Some(split_at) = last_space {
                let rest = line.split_off(split_at + 1);
                line.pop();
                out.push(rebuild_spans(&line));
                line = rest;
            } else {
                let overflow = line.pop();
                out.push(rebuild_spans(&line));
                line.clear();
                if let Some(unit) = overflow {
                    line.push(unit);
                }
            }
            line_width = line.iter().map(|(_, _, width)| *width).sum();
            last_space = None;
        }
    }
    out.push(rebuild_spans(&line));
    out
}

fn rebuild_spans(units: &[(String, Style, usize)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut buffer = String::new();
    for (text, style, _) in units {
        if current_style != Some(*style) {
            if let Some(style) = current_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buffer), style));
            }
            current_style = Some(*style);
        }
        buffer.push_str(text);
    }
    if let Some(style) = current_style {
        spans.push(Span::styled(buffer, style));
    }
    spans
}

fn safe_width_usize(width: u16) -> usize {
    usize::from(safe_cell_width(width))
}

fn append_with_newline(target: &mut String, value: &str) {
    if !target.is_empty() && !value.is_empty() {
        target.push('\n');
    }
    target.push_str(value);
}

fn bounded_text(text: String, max_chars: usize) -> String {
    bounded_sanitize(&text, max_chars)
}

fn append_bounded_text(target: &mut String, value: &str, max_chars: usize) {
    if value.is_empty() {
        return;
    }
    target.push_str(value);
    *target = bounded_sanitize(target, max_chars);
}

fn append_bounded_text_with_newline(target: &mut String, value: &str, max_chars: usize) {
    if value.is_empty() {
        return;
    }
    append_with_newline(target, value);
    *target = bounded_sanitize(target, max_chars);
}

fn diff_summary(diff: &crate::diff::Diff) -> String {
    use crate::diff::Row;
    let added = diff
        .rows
        .iter()
        .filter(|row| matches!(row, Row::Add { .. }))
        .count();
    let removed = diff
        .rows
        .iter()
        .filter(|row| matches!(row, Row::Remove { .. }))
        .count();
    let hidden = diff
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Truncated(n) => Some(*n),
            _ => None,
        })
        .sum::<usize>();

    if added == 0 && removed == 0 {
        return "Preview only".to_string();
    }

    let mut parts = vec![format!("+{added}"), format!("-{removed}")];
    if hidden > 0 {
        parts.push(format!("+{hidden} hidden"));
    }
    parts.join(", ")
}

fn render_file_diff(diff: &crate::diff::Diff, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    use crate::diff::Row;
    let max_width = safe_width_usize(width).saturating_sub(4).max(1);
    let mut lines = Vec::new();
    for row in &diff.rows {
        match row {
            Row::Add { lineno, segs } => {
                lines.push(diff_line(
                    *lineno,
                    "+",
                    diff_text(segs),
                    theme.diff_add(),
                    max_width,
                ));
            }
            Row::Remove { lineno, segs } => {
                lines.push(diff_line(
                    *lineno,
                    "-",
                    diff_text(segs),
                    theme.diff_remove(),
                    max_width,
                ));
            }
            Row::Context { lineno, text } => {
                lines.push(diff_line(
                    *lineno,
                    " ",
                    text.clone(),
                    theme.dim(),
                    max_width,
                ));
            }
            Row::Gap => lines.push(Line::from(vec![
                Span::styled("    ", theme.faint()),
                Span::styled("⋮", theme.faint()),
            ])),
            Row::Truncated(n) => lines.push(Line::from(vec![
                Span::styled("    ", theme.faint()),
                Span::styled(format!("… +{n} lines"), theme.faint()),
            ])),
        }
    }
    lines
}

fn diff_line(
    lineno: Option<usize>,
    sign: &str,
    text: String,
    style: Style,
    max_width: usize,
) -> Line<'static> {
    let number = lineno
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    Line::from(vec![
        Span::styled(number, Style::default().add_modifier(Modifier::DIM)),
        Span::styled(format!(" {sign} "), style),
        Span::styled(measure::truncate(&text, max_width), style),
    ])
}

fn diff_text(segs: &[crate::diff::Seg]) -> String {
    segs.iter().map(|seg| seg.text.as_str()).collect()
}

fn sanitize_label(text: &str) -> String {
    let clean = sanitize(text);
    let mut out = String::new();
    let mut pending_space = false;

    for ch in clean.chars() {
        if is_label_format_control(ch) {
            continue;
        }
        if ch.is_whitespace() || matches!(ch, '\u{2028}' | '\u{2029}') {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }

    let capped = out.trim().chars().take(MAX_LABEL_CHARS).collect::<String>();
    measure::truncate(&capped, MAX_LABEL_WIDTH)
}

fn is_label_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

fn first_non_empty_line(content: &str) -> String {
    let mut line = String::new();
    let mut line_width = 0usize;
    let preview_input: String = content.chars().take(MAX_TOOL_PREVIEW_SCAN_CHARS).collect();
    let mut chars = preview_input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => skip_escape_sequence(&mut chars),
            '\u{9b}' => drain_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{9e}' | '\u{9f}' => drain_to_st(&mut chars),
            '\r' => {}
            '\n' => {
                if !line.trim().is_empty() {
                    break;
                }
                line.clear();
                line_width = 0;
            }
            '\t' => {
                push_preview_char(&mut line, &mut line_width, ' ');
                push_preview_char(&mut line, &mut line_width, ' ');
                push_preview_char(&mut line, &mut line_width, ' ');
                push_preview_char(&mut line, &mut line_width, ' ');
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => push_preview_char(&mut line, &mut line_width, c),
        }

        if line_width >= MAX_TOOL_DETAIL_WIDTH && !line.trim().is_empty() {
            break;
        }
    }

    measure::truncate(line.trim(), MAX_TOOL_DETAIL_WIDTH)
}

fn clean_exec_command(command: &str) -> String {
    let clean = bounded_exec_text(command);
    let lines = clean
        .lines()
        .map(str::trim_end)
        .map(|line| measure::truncate(line, MAX_EXEC_COMMAND_WIDTH))
        .collect::<Vec<_>>();
    let command = lines.join("\n");
    if command.trim().is_empty() {
        "(unknown command)".to_string()
    } else {
        command
    }
}

fn command_logical_lines(title: &str, command: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut command_lines = command.lines();
    let first = command_lines.next().unwrap_or("(unknown command)");
    lines.push(Line::from(vec![
        Span::styled(
            format!("{title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(first.to_string()),
    ]));
    lines.extend(command_lines.map(|line| Line::from(Span::raw(line.to_string()))));
    lines
}

fn operation_line(kind: &str, target: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            kind.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(target.to_string()),
    ])
}

fn bounded_exec_text(text: &str) -> String {
    bounded_sanitize(text, MAX_EXEC_OUTPUT_SCAN_CHARS)
}

fn bound_exec_output_buffer(text: &str) -> String {
    let clean = sanitize(text);
    let total = clean.chars().count();
    if total <= MAX_EXEC_OUTPUT_SCAN_CHARS {
        return clean;
    }

    let marker = "\n… output truncated\n";
    let marker_width = marker.chars().count();
    let budget = MAX_EXEC_OUTPUT_SCAN_CHARS
        .saturating_sub(marker_width)
        .max(2);
    let head_budget = budget / 2;
    let tail_budget = budget.saturating_sub(head_budget);
    let head = clean.chars().take(head_budget).collect::<String>();
    let tail = clean
        .chars()
        .rev()
        .take(tail_budget)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head}{marker}{tail}")
}

fn bounded_output_lines(output: &str) -> Vec<String> {
    let clean = bound_exec_output_buffer(output);
    let lines = clean
        .lines()
        .map(str::trim_end)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let total = lines.len();
    let edge_total = EXEC_OUTPUT_HEAD_LINES + EXEC_OUTPUT_TAIL_LINES;
    if total <= edge_total + 1 {
        return lines;
    }

    let mut out = Vec::new();
    out.extend(lines.iter().take(EXEC_OUTPUT_HEAD_LINES).cloned());
    out.push(format!("… +{} lines", total.saturating_sub(edge_total)));
    out.extend(
        lines
            .iter()
            .skip(total.saturating_sub(EXEC_OUTPUT_TAIL_LINES))
            .cloned(),
    );
    out
}

fn classify_exec_command(command: &str) -> ExecCommandKind {
    let first_line = command.lines().next().unwrap_or(command).trim();
    let parts = first_line.split_whitespace().collect::<Vec<_>>();
    let Some(raw_program) = parts.first() else {
        return ExecCommandKind::Run("(unknown command)".to_string());
    };
    let program = raw_program
        .trim_matches(['"', '\''])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw_program)
        .to_ascii_lowercase();

    match program.as_str() {
        "cat" | "type" | "get-content" | "gc" => ExecCommandKind::Read(sanitize_exec_target(
            last_non_option(&parts).unwrap_or(first_line),
        )),
        "sed" => ExecCommandKind::Read(sanitize_exec_target(
            last_non_option(&parts).unwrap_or(first_line),
        )),
        "ls" | "dir" | "find" | "fd" | "get-childitem" | "gci" => {
            ExecCommandKind::List(sanitize_exec_target(last_non_option(&parts).unwrap_or(".")))
        }
        "rg" | "grep" | "select-string" => ExecCommandKind::Search(sanitize_exec_target(
            search_target(&parts).unwrap_or(first_line),
        )),
        _ => ExecCommandKind::Run(measure::truncate(first_line, MAX_EXEC_COMMAND_WIDTH)),
    }
}

fn sanitize_exec_target(target: &str) -> String {
    let target = sanitize_label(target);
    if target.is_empty() {
        "(unknown)".to_string()
    } else {
        target
    }
}

fn last_non_option<'a>(parts: &'a [&str]) -> Option<&'a str> {
    parts
        .iter()
        .skip(1)
        .rev()
        .copied()
        .find(|part| !part.starts_with('-'))
}

fn search_target<'a>(parts: &'a [&str]) -> Option<&'a str> {
    parts
        .iter()
        .copied()
        .skip(1)
        .find(|part| !part.starts_with('-'))
        .or_else(|| last_non_option(parts))
}

fn exec_command_from_tool(name: &str, input: &Value) -> Option<String> {
    if name != "bash" {
        return None;
    }
    input
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn is_tool_item(item: &TranscriptItem) -> bool {
    matches!(
        &item.payload,
        TranscriptPayload::ToolCall { .. } | TranscriptPayload::ToolResult { .. }
    ) || matches!(
        item.kind,
        TranscriptItemKind::ToolCall | TranscriptItemKind::ToolResult
    )
}

fn is_exec_item(item: &TranscriptItem) -> bool {
    matches!(
        &item.payload,
        TranscriptPayload::ExecCommand { .. } | TranscriptPayload::ExecOutput { .. }
    ) || matches!(item.kind, TranscriptItemKind::ExecCommand)
}

fn active_exec_should_defer(cell: &HistoryCellKind) -> bool {
    matches!(cell, HistoryCellKind::Exec(exec) if exec.should_defer_finalization())
}

fn plain_line(line: Line<'static>) -> String {
    line.spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

fn bounded_sanitize(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
    }
    if truncated {
        out.push_str("\n… truncated");
    }
    sanitize(&out)
}

fn render_streaming_markdown(
    stable_prefix: &str,
    mutable_tail: &str,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let stable = bounded_sanitize(stable_prefix, MAX_MARKDOWN_SOURCE_CHARS);
    let tail_budget = MAX_MARKDOWN_SOURCE_CHARS.saturating_sub(stable.chars().count());
    let tail = bounded_sanitize(mutable_tail, tail_budget);
    let mut lines = Vec::new();

    if !stable.trim().is_empty() {
        lines.extend(crate::markdown::render_markdown_with_highlight(
            &stable, theme, width, false,
        ));
    }

    if !tail.trim().is_empty() {
        let mut tail_lines =
            crate::markdown::render_markdown_with_highlight(&tail, theme, width, false);
        if let Some(first) = tail_lines.first_mut() {
            first.spans.insert(0, Span::styled("… ", theme.faint()));
        }
        lines.extend(tail_lines);
    }

    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn push_preview_char(line: &mut String, line_width: &mut usize, ch: char) {
    if *line_width >= MAX_TOOL_DETAIL_WIDTH {
        return;
    }
    let width = measure::char_width(ch);
    if line_width.saturating_add(width) > MAX_TOOL_DETAIL_WIDTH {
        return;
    }
    line.push(ch);
    *line_width += width;
}

fn skip_escape_sequence<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            drain_csi(chars);
        }
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
            chars.next();
            drain_to_st(chars);
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn drain_csi<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    for ch in chars.by_ref() {
        if ('@'..='~').contains(&ch) {
            break;
        }
    }
}

fn drain_to_st<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{07}' {
            break;
        }
        if ch == '\x1b' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        TranscriptExecSource, TranscriptExecStream, TranscriptItem, TranscriptItemKind,
        TranscriptItemStatus, TranscriptLifecycle, TranscriptPayload, TranscriptRole,
        TranscriptUpdate,
    };

    struct HugeCell;

    impl HistoryCell for HugeCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            vec![Line::default(); u16::MAX as usize + 1]
        }
    }

    #[test]
    fn desired_height_saturates_at_u16_max() {
        assert_eq!(HugeCell.desired_height(80), u16::MAX);
    }

    #[test]
    fn base_cells_render_prefixes_and_clamp_tiny_width() {
        let cells = CompositeCell::new(vec![
            HistoryCellKind::User(UserCell::new("hello world")),
            HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new("**ok**", false)),
            HistoryCellKind::Reasoning(ReasoningCell::new("checking")),
            HistoryCellKind::Notice(NoticeCell::new("notice")),
            HistoryCellKind::Error(ErrorCell::new("error")),
        ]);

        let text = flatten(&cells.display_lines(40));
        assert!(text.iter().any(|line| line.starts_with('›')));
        assert!(text.iter().any(|line| line.starts_with('●')));
        assert!(text.iter().any(|line| line.contains("reasoning")));
        assert!(cells.desired_height(0) >= 1);
    }

    #[test]
    fn parity_snapshot_matrix_covers_critical_flows() {
        let mut exec_done = ExecCell::command_with_id(
            Some(TranscriptItemId::local("exec", 1)),
            "cargo test -p agent-tui",
            TranscriptExecSource::Agent,
            TranscriptItemStatus::Running,
        );
        exec_done.apply_item(&TranscriptItem::new(
            Some(TranscriptItemId::local("exec", 1)),
            TranscriptRole::Assistant,
            TranscriptItemKind::ExecCommand,
            TranscriptItemStatus::Complete,
            TranscriptPayload::ExecOutput {
                content: "test result: ok".into(),
                is_error: false,
                stream: TranscriptExecStream::Combined,
                untrusted: true,
            },
        ));

        let write_cell = FileChangeCell::from_tool(
            "write",
            &serde_json::json!({ "path": "src/new.rs", "content": "fn main() {}\n" }),
            "created",
            false,
        )
        .expect("write file change");
        let edit_cell = FileChangeCell::from_tool(
            "edit",
            &serde_json::json!({
                "path": "src/lib.rs",
                "old_string": "old()",
                "new_string": "new()",
            }),
            "edited",
            false,
        )
        .expect("edit file change");
        let failed_edit = FileChangeCell::from_tool(
            "edit",
            &serde_json::json!({
                "path": "src/lib.rs",
                "old_string": "missing",
                "new_string": "new()",
            }),
            "anchor missing",
            true,
        )
        .expect("failed file change");

        let snapshots = vec![
            cell_snapshot(
                "chat idle",
                HistoryCellKind::Notice(NoticeCell::new("idle")),
            ),
            cell_snapshot(
                "user message",
                HistoryCellKind::User(UserCell::new("hello")),
            ),
            cell_snapshot(
                "streaming deltas",
                HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new("partial", true)),
            ),
            cell_snapshot(
                "assistant markdown",
                HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new("final **answer**", false)),
            ),
            cell_snapshot(
                "markdown code",
                HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new(
                    "```rust\nfn main() {}\n```",
                    false,
                )),
            ),
            cell_snapshot(
                "markdown table",
                HistoryCellKind::AgentMarkdown(AgentMarkdownCell::new(
                    "| A | B |\n|---|---|\n| 1 | 2 |",
                    false,
                )),
            ),
            cell_snapshot(
                "reasoning",
                HistoryCellKind::Reasoning(ReasoningCell::new("plan")),
            ),
            cell_snapshot("notice", HistoryCellKind::Notice(NoticeCell::new("notice"))),
            cell_snapshot("error", HistoryCellKind::Error(ErrorCell::new("failure"))),
            cell_snapshot(
                "exec running",
                HistoryCellKind::Exec(ExecCell::command_with_id(
                    Some(TranscriptItemId::local("exec", 2)),
                    "rg TODO",
                    TranscriptExecSource::Agent,
                    TranscriptItemStatus::Running,
                )),
            ),
            cell_snapshot("exec complete", HistoryCellKind::Exec(exec_done)),
            cell_snapshot(
                "exec orphan",
                HistoryCellKind::Exec(ExecCell::orphan(
                    "completed without start",
                    false,
                    TranscriptItemStatus::Complete,
                )),
            ),
            cell_snapshot(
                "tool calling",
                HistoryCellKind::Tool(ToolCell::calling(
                    "read",
                    &serde_json::json!({ "path": "src/lib.rs" }),
                    TranscriptItemStatus::Running,
                )),
            ),
            cell_snapshot(
                "tool success",
                HistoryCellKind::Tool(ToolCell::result(
                    "ok",
                    false,
                    TranscriptItemStatus::Complete,
                )),
            ),
            cell_snapshot(
                "tool error",
                HistoryCellKind::Tool(ToolCell::result(
                    "denied",
                    true,
                    TranscriptItemStatus::Failed,
                )),
            ),
            cell_snapshot("diff added", HistoryCellKind::FileChange(write_cell)),
            cell_snapshot("diff edited", HistoryCellKind::FileChange(edit_cell)),
            cell_snapshot("diff failure", HistoryCellKind::FileChange(failed_edit)),
            cell_snapshot(
                "approval",
                HistoryCellKind::Notice(NoticeCell::new("permission : bash")),
            ),
            cell_snapshot(
                "queue input resize",
                HistoryCellKind::Composite(CompositeCell::new(vec![
                    HistoryCellKind::User(UserCell::new("queued")),
                    HistoryCellKind::Notice(NoticeCell::new("Message ajouté à la file d'attente.")),
                ])),
            ),
        ];

        assert_eq!(snapshots.len(), 20);
        for (name, text) in snapshots {
            assert!(!text.trim().is_empty(), "empty snapshot for {name}");
        }
    }

    #[test]
    fn chat_surface_flushes_active_cell_once() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::local("assistant", 1);

        surface.apply_update(text_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "bon",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(text_update(
            TranscriptLifecycle::Delta,
            Some(id.clone()),
            "jour",
            TranscriptItemStatus::Running,
        ));

        assert_eq!(surface.active_revision(), Some(2));
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::AssistantMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Empty,
            ),
        });

        assert_eq!(surface.transcript_cells().len(), 1);
        assert!(surface.active_cell().is_none());
        let insert = surface
            .drain_pending_insert(40, InsertHistoryMode::InlineScrollback)
            .expect("finalized cell queues scrollback insert");
        assert_eq!(insert.mode, InsertHistoryMode::InlineScrollback);
        assert!(
            insert
                .lines
                .iter()
                .any(|line| line.as_str().contains("bonjour"))
        );
        assert!(
            surface
                .drain_pending_insert(20, InsertHistoryMode::InlineScrollback)
                .is_none(),
            "resize without new finalization must not duplicate inserts"
        );
    }

    #[test]
    fn active_tool_delta_only_changes_active_revision() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("tool", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "cargo test" }),
                },
            ),
        });

        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Delta,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolResult {
                    content: "line 1".into(),
                    is_error: false,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });

        assert_eq!(surface.transcript_cells().len(), 0);
        assert_eq!(surface.active_revision(), Some(2));
        let active = surface.active_cell().expect("tool remains active");
        assert!(
            flatten(&active.cell.display_lines(80))
                .join("\n")
                .contains("line 1")
        );
    }

    #[test]
    fn exec_cell_groups_command_output_and_completion() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "call-1");
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "cargo test",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Delta,
            Some(id.clone()),
            "compiling\n",
            false,
            TranscriptItemStatus::Running,
        ));

        assert_eq!(surface.active_revision(), Some(2));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(id),
            "ok",
            false,
            TranscriptItemStatus::Complete,
        ));

        assert_eq!(surface.transcript_cells().len(), 1);
        assert!(surface.active_cell().is_none());
        let text = flatten(&surface.display_lines(80)).join("\n");
        assert!(text.contains("Ran cargo test"));
        assert!(text.contains("compiling"));
        assert!(text.contains("ok"));
    }

    #[test]
    fn exec_output_uses_head_tail_truncation() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "call-1");
        let output = (1..=10)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "seq 1 10",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(id),
            &output,
            false,
            TranscriptItemStatus::Complete,
        ));

        let text = flatten(&surface.display_lines(80)).join("\n");
        assert!(text.contains("1"));
        assert!(text.contains("2"));
        assert!(text.contains("… +6 lines"));
        assert!(text.contains("9"));
        assert!(text.contains("10"));
        assert!(!text.lines().any(|line| line.trim() == "5"));
    }

    #[test]
    fn exec_output_buffer_is_bounded_across_many_deltas() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "call-1");
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "cat huge.log",
            TranscriptItemStatus::Running,
        ));
        for chunk in 0..4 {
            let mut content = "x".repeat(MAX_EXEC_OUTPUT_SCAN_CHARS / 2);
            content.push_str(&format!("\ntail-{chunk}"));
            surface.apply_update(exec_output_update(
                TranscriptLifecycle::Delta,
                Some(id.clone()),
                &content,
                false,
                TranscriptItemStatus::Running,
            ));
        }

        let active = surface.active_cell().expect("active exec");
        let HistoryCellKind::Exec(exec) = &active.cell else {
            assert!(matches!(&active.cell, HistoryCellKind::Exec(_)));
            return;
        };
        assert!(exec.calls[0].output.chars().count() <= MAX_EXEC_OUTPUT_SCAN_CHARS);
        assert!(exec.calls[0].output.contains("output truncated"));
        assert!(exec.calls[0].output.contains("tail-3"));
    }

    #[test]
    fn deferred_exploration_finalizes_before_non_group_exec() {
        let mut surface = ChatSurface::new();
        let search_id = TranscriptItemId::derived("exec", "search");
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(search_id.clone()),
            "rg shimmer",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(search_id),
            "",
            false,
            TranscriptItemStatus::Complete,
        ));

        let run_id = TranscriptItemId::derived("exec", "run");
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(run_id.clone()),
            "echo after",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(run_id),
            "after",
            false,
            TranscriptItemStatus::Complete,
        ));

        let text = flatten(&surface.display_lines(100)).join("\n");
        let explored = text.find("Explored").expect("explored cell");
        let ran = text.find("Ran echo after").expect("run cell");
        assert!(explored < ran);
    }

    #[test]
    fn failed_exploratory_exec_shows_output() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "search");
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "rg missing",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(id),
            "No files were searched",
            true,
            TranscriptItemStatus::Failed,
        ));

        assert_eq!(surface.transcript_cells().len(), 1);
        assert!(surface.active_cell().is_none());
        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Ran rg missing"));
        assert!(text.contains("No files were searched"));
        assert!(!text.contains("Explored"));
    }

    #[test]
    fn exploratory_exec_commands_group_until_boundary() {
        let mut surface = ChatSurface::new();
        for (id, command) in [
            ("call-1", "rg shimmer_spans"),
            ("call-2", "cat shimmer.rs"),
            ("call-3", "cat status_indicator_widget.rs"),
        ] {
            let id = TranscriptItemId::derived("exec", id);
            surface.apply_update(exec_command_update(
                TranscriptLifecycle::Started,
                Some(id.clone()),
                command,
                TranscriptItemStatus::Running,
            ));
            surface.apply_update(exec_output_update(
                TranscriptLifecycle::Completed,
                Some(id),
                "",
                false,
                TranscriptItemStatus::Complete,
            ));
        }

        assert_eq!(surface.transcript_cells().len(), 0);
        let active = flatten(&surface.active_display_lines(100)).join("\n");
        assert!(active.contains("Explored"));
        assert!(active.contains("Search shimmer_spans"));
        assert!(active.contains("Read shimmer.rs, status_indicator_widget.rs"));

        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("turn", 1)),
                TranscriptRole::System,
                TranscriptItemKind::TurnBoundary,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Empty,
            ),
        });

        assert_eq!(surface.transcript_cells().len(), 1);
        assert!(surface.active_cell().is_none());
    }

    #[test]
    fn orphan_exec_completion_degrades_without_panic() {
        let mut surface = ChatSurface::new();
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(TranscriptItemId::derived("exec", "missing")),
            "done",
            false,
            TranscriptItemStatus::Complete,
        ));

        let text = flatten(&surface.display_lines(40)).join("\n");
        assert!(text.contains("Ran (unknown command)"));
        assert!(text.contains("done"));
    }

    #[test]
    fn tool_title_is_sanitized_before_live_render() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(TranscriptItemId::derived("tool", "call-1")),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "bash".into(),
                    input: serde_json::json!({
                        "command": "echo ok\u{1b}]52;c;AAAA\u{7}\u{1b}]8;;http://evil\u{1b}\\link\u{1b}[31m"
                    }),
                },
            ),
        });

        let rendered = flatten(&surface.active_display_lines(120)).join("\n");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("52;c"));
        assert!(!rendered.contains("http://evil"));
        assert!(rendered.contains("echo ok"));
    }

    #[test]
    fn tool_result_preview_is_bounded_before_full_sanitize() {
        let content = format!(
            "{}visible output that should be kept",
            "\u{1b}]52;c;AAAA\u{7}".repeat(100)
        );
        let preview = first_non_empty_line(&content);

        assert!(preview.contains("visible output"));
        assert!(!preview.contains("52;c"));
        assert!(measure::width(&preview) <= MAX_TOOL_DETAIL_WIDTH);
    }

    #[test]
    fn agent_markdown_cell_bounds_source_before_rendering() {
        let cell = AgentMarkdownCell::new("x".repeat(MAX_MARKDOWN_SOURCE_CHARS + 100), false);
        let rendered = flatten(&cell.display_lines(80)).join("\n");

        assert!(rendered.contains("truncated"));
    }

    #[test]
    fn streaming_cell_exposes_stable_prefix_and_mutable_tail() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::local("assistant", 1);
        surface.apply_update(text_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "stable line\n",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(text_update(
            TranscriptLifecycle::Delta,
            Some(id),
            "| A | B |\n|---|---|\n| 1",
            TranscriptItemStatus::Running,
        ));

        let active = surface.active_cell().expect("active assistant cell");
        let HistoryCellKind::AgentMarkdown(cell) = &active.cell else {
            assert!(
                matches!(&active.cell, HistoryCellKind::AgentMarkdown(_)),
                "assistant markdown cell expected"
            );
            return;
        };
        let view = cell.stream_view();
        assert_eq!(view.stable_prefix, "stable line\n");
        assert!(view.mutable_tail.contains("| A | B |"));
        assert!(flatten(&cell.display_lines(80)).join("\n").contains("…"));
    }

    #[test]
    fn streaming_cell_reflows_from_raw_source_on_resize() {
        let cell = AgentMarkdownCell::new(
            "un texte long qui doit wrapper differemment selon la largeur",
            true,
        );

        let wide = cell.display_lines(80).len();
        let narrow = cell.display_lines(14).len();

        assert!(narrow > wide);
        assert_eq!(
            cell.stream_view().raw_source,
            "un texte long qui doit wrapper differemment selon la largeur"
        );
    }

    #[test]
    fn simple_conversation_keeps_user_agent_tool_agent_order() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("user", 1)),
                TranscriptRole::User,
                TranscriptItemKind::UserMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Text { delta: "q".into() },
            ),
        });
        surface.apply_update(text_update(
            TranscriptLifecycle::Completed,
            Some(TranscriptItemId::local("assistant", 2)),
            "a1",
            TranscriptItemStatus::Complete,
        ));
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::derived("tool", "call-1")),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Complete,
                TranscriptPayload::ToolResult {
                    content: "ok".into(),
                    is_error: false,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });
        surface.apply_update(text_update(
            TranscriptLifecycle::Completed,
            Some(TranscriptItemId::local("assistant", 3)),
            "a2",
            TranscriptItemStatus::Complete,
        ));

        let text = flatten(&surface.display_lines(80)).join("\n");
        assert!(text.find('q') < text.find("a1"));
        assert!(text.find("a1") < text.find("Tool result"));
        assert!(text.find("Tool result") < text.find("a2"));
    }

    #[test]
    fn replay_from_messages_builds_cells_and_queues_single_insert() {
        let messages = vec![
            Message::user("salut"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "plan".into(),
                },
                ContentBlock::Text {
                    text: "voici".into(),
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.rs" }),
                },
            ]),
            Message::tool_result("orphan", "contenu", false),
        ];

        let mut surface = ChatSurface::from_messages(&messages);
        let text = flatten(&surface.display_lines(80)).join("\n");
        assert!(text.contains("salut"));
        assert!(text.contains("plan"));
        assert!(text.contains("voici"));
        assert!(text.contains("Read(a.rs)"));
        assert!(text.contains("contenu"));
        let initial = surface
            .drain_pending_insert(80, InsertHistoryMode::InlineScrollback)
            .expect("replay queues one initial scrollback insert");
        assert!(
            initial
                .lines
                .iter()
                .any(|line| line.as_str().contains("salut"))
        );
        assert!(
            surface
                .drain_pending_insert(80, InsertHistoryMode::InlineScrollback)
                .is_none()
        );
    }

    #[test]
    fn replay_pairs_bash_tool_use_with_exec_result() {
        let messages = vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "bash-1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "echo ok" }),
            }]),
            Message::tool_result("bash-1", "ok", false),
        ];

        let surface = ChatSurface::from_messages(&messages);
        let text = flatten(&surface.display_lines(80)).join("\n");

        assert!(text.contains("Ran echo ok"));
        assert!(text.contains("ok"));
        assert!(!text.contains("Tool result"));
    }

    #[test]
    fn active_display_lines_omits_finalized_scrollback() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("user", 1)),
                TranscriptRole::User,
                TranscriptItemKind::UserMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Text {
                    delta: "already inserted".into(),
                },
            ),
        });
        let _ = surface.drain_pending_insert(80, InsertHistoryMode::InlineScrollback);
        surface.apply_update(text_update(
            TranscriptLifecycle::Started,
            Some(TranscriptItemId::local("assistant", 2)),
            "live tail",
            TranscriptItemStatus::Running,
        ));

        let active = flatten(&surface.active_display_lines(80)).join("\n");
        assert!(active.contains("live tail"));
        assert!(!active.contains("already inserted"));
    }

    #[test]
    fn stream_reset_drops_active_tail() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::local("assistant", 1);
        surface.apply_update(text_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "draft",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Reset,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::AssistantMessage,
                TranscriptItemStatus::Cancelled,
                TranscriptPayload::Empty,
            ),
        });

        assert!(surface.active_cell().is_none());
        assert!(surface.transcript_cells().is_empty());
    }

    #[test]
    fn stream_reset_without_id_drops_active_tail() {
        let mut surface = ChatSurface::new();
        surface.apply_update(text_update(
            TranscriptLifecycle::Started,
            Some(TranscriptItemId::local("assistant", 1)),
            "draft",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Reset,
            item: TranscriptItem::new(
                None,
                TranscriptRole::Assistant,
                TranscriptItemKind::AssistantMessage,
                TranscriptItemStatus::Cancelled,
                TranscriptPayload::Empty,
            ),
        });

        assert!(surface.active_cell().is_none());
        assert!(surface.transcript_cells().is_empty());
    }

    #[test]
    fn successful_edit_tool_renders_file_change_diff() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("tool", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/main.rs",
                        "old_string": "let x = 1;",
                        "new_string": "let x = 2;"
                    }),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Complete,
                TranscriptPayload::ToolResult {
                    content: "edited".into(),
                    is_error: false,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });

        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Edited src/main.rs"));
        assert!(text.contains("let x = 1;"));
        assert!(text.contains("let x = 2;"));
    }

    #[test]
    fn failed_edit_tool_renders_failure_without_diff() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("tool", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/main.rs",
                        "old_string": "old",
                        "new_string": "new"
                    }),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Failed,
                TranscriptPayload::ToolResult {
                    content: "anchor missing".into(),
                    is_error: true,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });

        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("File change failed src/main.rs"));
        assert!(text.contains("anchor missing"));
        assert!(text.contains("Failed"));
        assert!(!text.contains("Called"));
        assert!(!text.contains("+ new"));
    }

    #[test]
    fn edit_tool_preserves_input_across_interleaved_permission() {
        let mut surface = ChatSurface::new();
        let tool_id = TranscriptItemId::derived("tool", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(tool_id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "old_string": "old()",
                        "new_string": "new()"
                    }),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(TranscriptItemId::derived("permission", "call-1")),
                TranscriptRole::System,
                TranscriptItemKind::PermissionRequest,
                TranscriptItemStatus::Running,
                TranscriptPayload::Permission {
                    tool: "edit".into(),
                    reason: "workspace write".into(),
                    taint_forced: false,
                    input_summary: "src/lib.rs".into(),
                    mode: "ask".into(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(tool_id),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolResult,
                TranscriptItemStatus::Complete,
                TranscriptPayload::ToolResult {
                    content: "edited".into(),
                    is_error: false,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });

        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Edited src/lib.rs"));
        assert!(text.contains("old()"));
        assert!(text.contains("new()"));
    }

    #[test]
    fn concurrent_edit_tools_keep_their_inputs_until_results_arrive() {
        let mut surface = ChatSurface::new();
        let first = TranscriptItemId::derived("tool", "call-1");
        let second = TranscriptItemId::derived("tool", "call-2");
        for (id, path, old_string, new_string) in [
            (&first, "a.rs", "let a = 1;", "let a = 2;"),
            (&second, "b.rs", "let b = 1;", "let b = 2;"),
        ] {
            surface.apply_update(TranscriptUpdate {
                lifecycle: TranscriptLifecycle::Started,
                item: TranscriptItem::new(
                    Some(id.clone()),
                    TranscriptRole::Assistant,
                    TranscriptItemKind::ToolCall,
                    TranscriptItemStatus::Running,
                    TranscriptPayload::ToolCall {
                        name: "edit".into(),
                        input: serde_json::json!({
                            "path": path,
                            "old_string": old_string,
                            "new_string": new_string
                        }),
                    },
                ),
            });
        }
        for id in [first, second] {
            surface.apply_update(TranscriptUpdate {
                lifecycle: TranscriptLifecycle::Completed,
                item: TranscriptItem::new(
                    Some(id),
                    TranscriptRole::Assistant,
                    TranscriptItemKind::ToolResult,
                    TranscriptItemStatus::Complete,
                    TranscriptPayload::ToolResult {
                        content: "edited".into(),
                        is_error: false,
                        error_kind: None,
                        untrusted: true,
                    },
                ),
            });
        }

        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Edited a.rs"));
        assert!(text.contains("let a = 1;"));
        assert!(text.contains("let a = 2;"));
        assert!(text.contains("Edited b.rs"));
        assert!(text.contains("let b = 1;"));
        assert!(text.contains("let b = 2;"));
    }

    #[test]
    fn file_change_path_label_collapses_newline_spoofing() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("tool", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/main.rs\nFile change failed fake",
                        "old_string": "old",
                        "new_string": "new"
                    }),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolResult,
                TranscriptItemStatus::Complete,
                TranscriptPayload::ToolResult {
                    content: "edited".into(),
                    is_error: false,
                    error_kind: None,
                    untrusted: true,
                },
            ),
        });

        let lines = flatten(&surface.display_lines(120));
        let text = lines.join("\n");
        assert!(text.contains("Edited src/main.rs File change failed fake"));
        assert!(
            !lines
                .iter()
                .any(|line| line.trim() == "File change failed fake")
        );
    }

    #[test]
    fn tool_title_collapses_newline_spoofing() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(TranscriptItemId::derived("tool", "call-1")),
                TranscriptRole::Assistant,
                TranscriptItemKind::ToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::ToolCall {
                    name: "bash".into(),
                    input: serde_json::json!({
                        "command": "echo ok\nTool failed fake"
                    }),
                },
            ),
        });

        let lines = flatten(&surface.active_display_lines(120));
        let text = lines.join("\n");
        assert!(text.contains("echo ok"));
        assert!(!text.contains("Tool failed fake"));
        assert!(!lines.iter().any(|line| line.trim() == "Tool failed fake"));
    }

    #[test]
    fn label_strips_bidi_and_zero_width_controls() {
        let label = sanitize_label("src/\u{202e}evil.rs\u{200b}");

        assert_eq!(label, "src/evil.rs");
    }

    #[test]
    fn non_exec_cells_bound_stored_text() {
        let mut cell = ReasoningCell::new("x".repeat(MAX_TEXT_CELL_CHARS + 100));
        assert!(cell.text.contains("truncated"));
        assert!(cell.text.chars().count() <= MAX_TEXT_CELL_CHARS + "\n… truncated".len());

        append_bounded_text(
            &mut cell.text,
            &"y".repeat(MAX_TEXT_CELL_CHARS),
            MAX_TEXT_CELL_CHARS,
        );
        assert!(cell.text.contains("truncated"));
        assert!(!cell.text.contains(&"y".repeat(100)));
    }

    fn text_update(
        lifecycle: TranscriptLifecycle,
        id: Option<TranscriptItemId>,
        delta: &str,
        status: TranscriptItemStatus,
    ) -> TranscriptUpdate {
        TranscriptUpdate {
            lifecycle,
            item: TranscriptItem::new(
                id,
                TranscriptRole::Assistant,
                TranscriptItemKind::AssistantMessage,
                status,
                TranscriptPayload::Text {
                    delta: delta.into(),
                },
            ),
        }
    }

    fn exec_command_update(
        lifecycle: TranscriptLifecycle,
        id: Option<TranscriptItemId>,
        command: &str,
        status: TranscriptItemStatus,
    ) -> TranscriptUpdate {
        TranscriptUpdate {
            lifecycle,
            item: TranscriptItem::new(
                id,
                TranscriptRole::Assistant,
                TranscriptItemKind::ExecCommand,
                status,
                TranscriptPayload::ExecCommand {
                    command: command.into(),
                    source: TranscriptExecSource::Agent,
                },
            ),
        }
    }

    fn exec_output_update(
        lifecycle: TranscriptLifecycle,
        id: Option<TranscriptItemId>,
        content: &str,
        is_error: bool,
        status: TranscriptItemStatus,
    ) -> TranscriptUpdate {
        TranscriptUpdate {
            lifecycle,
            item: TranscriptItem::new(
                id,
                TranscriptRole::Assistant,
                TranscriptItemKind::ExecCommand,
                status,
                TranscriptPayload::ExecOutput {
                    content: content.into(),
                    is_error,
                    stream: TranscriptExecStream::Combined,
                    untrusted: true,
                },
            ),
        }
    }

    fn flatten(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn cell_snapshot(name: &'static str, cell: HistoryCellKind) -> (&'static str, String) {
        let lines = cell.display_lines(if name.contains("resize") { 24 } else { 80 });
        (name, flatten(&lines).join("\n"))
    }
}
