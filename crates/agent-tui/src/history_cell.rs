//! Finalized transcript cells for the Codex TUI parity path.
//!
//! `HistoryCell` is the rendering boundary for committed transcript content. It
//! stays pure: no terminal I/O, no core mutation, and no ANSI coming from
//! `agent-core`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agent_core::message::{ContentBlock, Message, Role, ToolCallId};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::app_event::{
    TranscriptExecSource, TranscriptExecStream, TranscriptHookOutputEntry,
    TranscriptHookOutputKind, TranscriptHookStatus, TranscriptItem, TranscriptItemId,
    TranscriptItemKind, TranscriptItemStatus, TranscriptLifecycle, TranscriptNoticeKind,
    TranscriptNoticeLink, TranscriptPatchChangeKind, TranscriptPatchFileChange, TranscriptPayload,
    TranscriptPlanStepStatus, TranscriptRole, TranscriptUpdate, TranscriptUserInputAnswer,
    TranscriptUserInputQuestion,
};
use crate::insert_history::{InsertHistoryMode, PendingHistoryInsert};
use crate::measure;
use crate::render::sanitize;
use crate::streaming::StreamController;
use crate::terminal_hyperlinks::{
    HyperlinkLine, annotate_web_urls, plain_hyperlink_lines, visible_lines,
};
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
const TOOL_CALL_MAX_LINES: usize = 5;
const USER_SHELL_TOOL_CALL_MAX_LINES: usize = 50;
const RAW_TOOL_OUTPUT_WIDTH: usize = 120;
const SESSION_HEADER_MAX_INNER_WIDTH: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRenderMode {
    Rich,
    Raw,
}

pub fn raw_lines_from_source(source: &str) -> Vec<Line<'static>> {
    if source.is_empty() {
        return Vec::new();
    }

    let mut parts = source.split('\n').collect::<Vec<_>>();
    if source.ends_with('\n') {
        parts.pop();
    }

    parts
        .into_iter()
        .map(|line| Line::from(line.to_string()))
        .collect()
}

pub fn plain_lines(lines: impl IntoIterator<Item = Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let text = line
                .spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>();
            Line::from(text)
        })
        .collect()
}

pub trait HistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        plain_hyperlink_lines(self.display_lines(width))
    }

    fn display_lines_for_mode(&self, width: u16, mode: HistoryRenderMode) -> Vec<Line<'static>> {
        match mode {
            HistoryRenderMode::Rich => visible_lines(self.display_hyperlink_lines(width)),
            HistoryRenderMode::Raw => self.raw_lines(),
        }
    }

    fn display_hyperlink_lines_for_mode(
        &self,
        width: u16,
        mode: HistoryRenderMode,
    ) -> Vec<HyperlinkLine> {
        match mode {
            HistoryRenderMode::Rich => self.display_hyperlink_lines(width),
            HistoryRenderMode::Raw => plain_hyperlink_lines(self.raw_lines()),
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.desired_height_for_mode(width, HistoryRenderMode::Rich)
    }

    fn desired_height_for_mode(&self, width: u16, mode: HistoryRenderMode) -> u16 {
        let rows = Paragraph::new(Text::from(
            self.display_lines_for_mode(width.max(MIN_CELL_WIDTH), mode),
        ))
        .wrap(Wrap { trim: false })
        .line_count(width.max(MIN_CELL_WIDTH))
        .max(1);
        rows.min(u16::MAX as usize) as u16
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        plain_hyperlink_lines(self.transcript_lines(width))
    }

    fn desired_transcript_height(&self, width: u16) -> u16 {
        let lines = visible_lines(self.transcript_hyperlink_lines(width.max(MIN_CELL_WIDTH)));
        if let [line] = &lines[..]
            && line
                .spans
                .iter()
                .all(|span| span.content.chars().all(char::is_whitespace))
        {
            return 1;
        }

        let rows = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .line_count(width.max(MIN_CELL_WIDTH))
            .max(1);
        rows.min(u16::MAX as usize) as u16
    }

    fn is_stream_continuation(&self) -> bool {
        false
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        None
    }
}

pub fn safe_cell_width(width: u16) -> u16 {
    width.max(MIN_CELL_WIDTH)
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryCellKind {
    SessionHeader(SessionHeaderCell),
    User(UserCell),
    AgentMarkdown(AgentMarkdownCell),
    Reasoning(ReasoningCell),
    Notice(NoticeCell),
    Error(ErrorCell),
    Exec(ExecCell),
    Tool(ToolCell),
    Approval(ApprovalCell),
    PlanUpdate(PlanUpdateCell),
    WebSearch(WebSearchCell),
    McpTool(McpToolCell),
    RequestUserInput(RequestUserInputCell),
    FinalSeparator(FinalMessageSeparatorCell),
    PatchSummary(PatchSummaryCell),
    PatchApplyFailure(PatchApplyFailureCell),
    SpecialNotice(SpecialNoticeCell),
    Hook(HookCell),
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
            Self::Approval(cell) => cell.apply_item(item),
            Self::Exec(cell) => cell.apply_item(item),
            Self::WebSearch(cell) => cell.apply_item(item),
            Self::McpTool(cell) => cell.apply_item(item),
            Self::RequestUserInput(_) => {}
            Self::PlanUpdate(_)
            | Self::FinalSeparator(_)
            | Self::PatchSummary(_)
            | Self::PatchApplyFailure(_)
            | Self::SpecialNotice(_)
            | Self::SessionHeader(_) => {}
            Self::Hook(cell) => cell.apply_item(item),
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
            Self::Approval(cell) => cell.status = status,
            Self::WebSearch(cell) => cell.mark_status(status),
            Self::McpTool(cell) => cell.mark_status(status),
            Self::Hook(cell) => cell.mark_status(status),
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
            Self::SessionHeader(cell) => cell.display_lines(width),
            Self::User(cell) => cell.display_lines(width),
            Self::AgentMarkdown(cell) => cell.display_lines(width),
            Self::Reasoning(cell) => cell.display_lines(width),
            Self::Notice(cell) => cell.display_lines(width),
            Self::Error(cell) => cell.display_lines(width),
            Self::Exec(cell) => cell.display_lines(width),
            Self::Tool(cell) => cell.display_lines(width),
            Self::Approval(cell) => cell.display_lines(width),
            Self::PlanUpdate(cell) => cell.display_lines(width),
            Self::WebSearch(cell) => cell.display_lines(width),
            Self::McpTool(cell) => cell.display_lines(width),
            Self::RequestUserInput(cell) => cell.display_lines(width),
            Self::FinalSeparator(cell) => cell.display_lines(width),
            Self::PatchSummary(cell) => cell.display_lines(width),
            Self::PatchApplyFailure(cell) => cell.display_lines(width),
            Self::SpecialNotice(cell) => cell.display_lines(width),
            Self::Hook(cell) => cell.display_lines(width),
            Self::FileChange(cell) => cell.display_lines(width),
            Self::Composite(cell) => cell.display_lines(width),
        }
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        match self {
            Self::SessionHeader(cell) => cell.raw_lines(),
            Self::User(cell) => cell.raw_lines(),
            Self::AgentMarkdown(cell) => cell.raw_lines(),
            Self::Reasoning(cell) => cell.raw_lines(),
            Self::Notice(cell) => cell.raw_lines(),
            Self::Error(cell) => cell.raw_lines(),
            Self::Exec(cell) => cell.raw_lines(),
            Self::Tool(cell) => cell.raw_lines(),
            Self::Approval(cell) => cell.raw_lines(),
            Self::PlanUpdate(cell) => cell.raw_lines(),
            Self::WebSearch(cell) => cell.raw_lines(),
            Self::McpTool(cell) => cell.raw_lines(),
            Self::RequestUserInput(cell) => cell.raw_lines(),
            Self::FinalSeparator(cell) => cell.raw_lines(),
            Self::PatchSummary(cell) => cell.raw_lines(),
            Self::PatchApplyFailure(cell) => cell.raw_lines(),
            Self::SpecialNotice(cell) => cell.raw_lines(),
            Self::Hook(cell) => cell.raw_lines(),
            Self::FileChange(cell) => cell.raw_lines(),
            Self::Composite(cell) => cell.raw_lines(),
        }
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        match self {
            Self::SessionHeader(cell) => cell.display_hyperlink_lines(width),
            Self::User(cell) => cell.display_hyperlink_lines(width),
            Self::AgentMarkdown(cell) => cell.display_hyperlink_lines(width),
            Self::Reasoning(cell) => cell.display_hyperlink_lines(width),
            Self::Notice(cell) => cell.display_hyperlink_lines(width),
            Self::Error(cell) => cell.display_hyperlink_lines(width),
            Self::Exec(cell) => cell.display_hyperlink_lines(width),
            Self::Tool(cell) => cell.display_hyperlink_lines(width),
            Self::Approval(cell) => cell.display_hyperlink_lines(width),
            Self::PlanUpdate(cell) => cell.display_hyperlink_lines(width),
            Self::WebSearch(cell) => cell.display_hyperlink_lines(width),
            Self::McpTool(cell) => cell.display_hyperlink_lines(width),
            Self::RequestUserInput(cell) => cell.display_hyperlink_lines(width),
            Self::FinalSeparator(cell) => cell.display_hyperlink_lines(width),
            Self::PatchSummary(cell) => cell.display_hyperlink_lines(width),
            Self::PatchApplyFailure(cell) => cell.display_hyperlink_lines(width),
            Self::SpecialNotice(cell) => cell.display_hyperlink_lines(width),
            Self::Hook(cell) => cell.display_hyperlink_lines(width),
            Self::FileChange(cell) => cell.display_hyperlink_lines(width),
            Self::Composite(cell) => cell.display_hyperlink_lines(width),
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::SessionHeader(cell) => cell.transcript_lines(width),
            Self::User(cell) => cell.transcript_lines(width),
            Self::AgentMarkdown(cell) => cell.transcript_lines(width),
            Self::Reasoning(cell) => cell.transcript_lines(width),
            Self::Notice(cell) => cell.transcript_lines(width),
            Self::Error(cell) => cell.transcript_lines(width),
            Self::Exec(cell) => cell.transcript_lines(width),
            Self::Tool(cell) => cell.transcript_lines(width),
            Self::Approval(cell) => cell.transcript_lines(width),
            Self::PlanUpdate(cell) => cell.transcript_lines(width),
            Self::WebSearch(cell) => cell.transcript_lines(width),
            Self::McpTool(cell) => cell.transcript_lines(width),
            Self::RequestUserInput(cell) => cell.transcript_lines(width),
            Self::FinalSeparator(cell) => cell.transcript_lines(width),
            Self::PatchSummary(cell) => cell.transcript_lines(width),
            Self::PatchApplyFailure(cell) => cell.transcript_lines(width),
            Self::SpecialNotice(cell) => cell.transcript_lines(width),
            Self::Hook(cell) => cell.transcript_lines(width),
            Self::FileChange(cell) => cell.transcript_lines(width),
            Self::Composite(cell) => cell.transcript_lines(width),
        }
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        match self {
            Self::SessionHeader(cell) => cell.transcript_hyperlink_lines(width),
            Self::User(cell) => cell.transcript_hyperlink_lines(width),
            Self::AgentMarkdown(cell) => cell.transcript_hyperlink_lines(width),
            Self::Reasoning(cell) => cell.transcript_hyperlink_lines(width),
            Self::Notice(cell) => cell.transcript_hyperlink_lines(width),
            Self::Error(cell) => cell.transcript_hyperlink_lines(width),
            Self::Exec(cell) => cell.transcript_hyperlink_lines(width),
            Self::Tool(cell) => cell.transcript_hyperlink_lines(width),
            Self::Approval(cell) => cell.transcript_hyperlink_lines(width),
            Self::PlanUpdate(cell) => cell.transcript_hyperlink_lines(width),
            Self::WebSearch(cell) => cell.transcript_hyperlink_lines(width),
            Self::McpTool(cell) => cell.transcript_hyperlink_lines(width),
            Self::RequestUserInput(cell) => cell.transcript_hyperlink_lines(width),
            Self::FinalSeparator(cell) => cell.transcript_hyperlink_lines(width),
            Self::PatchSummary(cell) => cell.transcript_hyperlink_lines(width),
            Self::PatchApplyFailure(cell) => cell.transcript_hyperlink_lines(width),
            Self::SpecialNotice(cell) => cell.transcript_hyperlink_lines(width),
            Self::Hook(cell) => cell.transcript_hyperlink_lines(width),
            Self::FileChange(cell) => cell.transcript_hyperlink_lines(width),
            Self::Composite(cell) => cell.transcript_hyperlink_lines(width),
        }
    }

    fn is_stream_continuation(&self) -> bool {
        matches!(self, Self::AgentMarkdown(cell) if cell.is_stream_continuation())
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        match self {
            Self::AgentMarkdown(cell) => cell.transcript_animation_tick(),
            Self::WebSearch(cell) => cell.transcript_animation_tick(),
            Self::McpTool(cell) => cell.transcript_animation_tick(),
            Self::Hook(cell) => cell.transcript_animation_tick(),
            _ => None,
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
        let text = sanitize(&self.text);
        let text = trim_trailing_blank_user_lines(&text);
        if text.is_empty() {
            return Vec::new();
        }

        let user_style = user_message_style();
        let lines = text_lines(&text, user_message_body_style());
        let mut out = Vec::new();
        out.push(user_message_padding_line(user_style));
        out.extend(style_user_message_lines(render_prefixed(
            &lines,
            Span::styled("› ", user_message_prefix_style()),
            Span::styled("  ", user_style),
            width,
        )));
        out.push(user_message_padding_line(user_style));
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.text)
    }
}

fn user_message_style() -> Style {
    Style::default().bg(user_message_bg())
}

fn user_message_bg() -> Color {
    Color::Rgb(0x32, 0x32, 0x36)
}

fn user_message_body_style() -> Style {
    Style::default()
        .fg(Color::Rgb(0xee, 0xee, 0xf0))
        .bg(user_message_bg())
}

fn user_message_prefix_style() -> Style {
    Style::default()
        .fg(Color::Rgb(0xb8, 0xb8, 0xbf))
        .bg(user_message_bg())
        .add_modifier(Modifier::BOLD)
}

fn user_message_padding_line(style: Style) -> Line<'static> {
    let mut line = Line::from("");
    line.style = style;
    line
}

fn trim_trailing_blank_user_lines(text: &str) -> String {
    let mut lines = text.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn style_user_message_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let style = user_message_style();
    lines
        .into_iter()
        .map(|mut line| {
            line.style = line.style.patch(style);
            line.spans = line
                .spans
                .into_iter()
                .map(|mut span| {
                    span.style = span.style.patch(style);
                    span
                })
                .collect();
            line
        })
        .collect()
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

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.markdown)
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        annotate_web_urls(self.display_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn is_stream_continuation(&self) -> bool {
        self.streaming
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        self.streaming.then_some(self.controller.revision())
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
        let display = reasoning_display_text(&self.text);
        let lines = reasoning_summary_lines(&display, width);
        render_prefixed(
            &lines,
            Span::styled("• ", reasoning_prefix_style()),
            Span::styled("  ", reasoning_prefix_style()),
            width,
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.text)
    }
}

fn reasoning_display_text(text: &str) -> String {
    let clean = bounded_sanitize(text, MAX_MARKDOWN_SOURCE_CHARS);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some((header, after_header)) = split_first_bold_markdown(trimmed) {
        let summary = after_header.trim();
        if summary.is_empty() {
            return header;
        }
        return summary.to_string();
    }

    trimmed.to_string()
}

fn split_first_bold_markdown(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    let mut open = 0usize;
    while open + 1 < bytes.len() {
        if bytes[open] == b'*' && bytes[open + 1] == b'*' {
            let start = open + 2;
            let mut close = start;
            while close + 1 < bytes.len() {
                if bytes[close] == b'*' && bytes[close + 1] == b'*' {
                    let header = text[start..close].trim();
                    if header.is_empty() {
                        return None;
                    }
                    return Some((header.to_string(), &text[(close + 2)..]));
                }
                close += 1;
            }
            return None;
        }
        open += 1;
    }
    None
}

fn reasoning_summary_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![Line::from(Span::styled(
            "Thinking",
            reasoning_summary_style(),
        ))];
    }

    let theme = Theme::new(true);
    let content_width = safe_width_usize(width).saturating_sub(2).max(1);
    crate::markdown::render_markdown(text, &theme, content_width)
        .into_iter()
        .map(style_reasoning_line)
        .collect()
}

fn style_reasoning_line(mut line: Line<'static>) -> Line<'static> {
    let style = reasoning_summary_style();
    line.spans = line
        .spans
        .into_iter()
        .map(|mut span| {
            span.style = span.style.patch(style);
            span
        })
        .collect();
    line
}

fn reasoning_summary_style() -> Style {
    Style::default()
        .fg(Color::Rgb(0xa8, 0xa8, 0xb0))
        .add_modifier(Modifier::ITALIC)
}

fn reasoning_prefix_style() -> Style {
    Style::default().fg(Color::Rgb(0x6f, 0x6f, 0x78))
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

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.message)
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

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.message)
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

    fn can_group_with(&self, _call: &ExecCall) -> bool {
        false
    }

    fn should_defer_finalization(&self) -> bool {
        false
    }

    fn call_mut(&mut self, id: Option<&TranscriptItemId>) -> Option<&mut ExecCall> {
        let id = id?;
        self.calls
            .iter_mut()
            .rev()
            .find(|call| call.id.as_ref() == Some(id))
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
}

impl HistoryCell for ExecCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.command_display_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.transcript_lines(u16::MAX))
    }

    fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (idx, call) in self.calls.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            out.push(Line::from(format!("$ {}", call.command)));
            out.extend(raw_lines_from_source(&call.output));
            out.push(Line::from(call.transcript_status_line()));
        }
        out
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
        Self {
            id,
            command,
            source,
            status,
            output: String::new(),
            is_error: false,
            stream: TranscriptExecStream::Combined,
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
                    exec_output_chrome_style(),
                ))],
                Span::styled("  └ ", exec_output_chrome_style()),
                Span::styled("    ", exec_output_chrome_style()),
                width,
            );
        }

        let line_limit = if self.source == TranscriptExecSource::User {
            USER_SHELL_TOOL_CALL_MAX_LINES
        } else {
            TOOL_CALL_MAX_LINES
        };
        let lines = bounded_output_lines(&self.output, line_limit)
            .into_iter()
            .map(|line| {
                let style = if is_output_hint_line(&line) {
                    exec_output_chrome_style()
                } else {
                    exec_output_body_style(self.failed())
                };
                Line::from(Span::styled(line, style))
            })
            .collect::<Vec<_>>();
        let prefixed = render_prefixed(
            &lines,
            Span::styled("  └ ", exec_output_chrome_style()),
            Span::styled("    ", exec_output_chrome_style()),
            width,
        );
        truncate_output_screen_lines(prefixed, line_limit)
    }

    fn transcript_status_line(&self) -> String {
        if self.is_running() {
            "running".to_string()
        } else if self.failed() {
            "failed".to_string()
        } else {
            "succeeded".to_string()
        }
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
            detail: String::new(),
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
            TranscriptItemStatus::Running => "Running",
            TranscriptItemStatus::Complete => "Ran",
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
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        );
        if let Some(file_change) = &self.file_change {
            out.extend(file_change.display_lines(width));
        }
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut source = self.title.clone();
        if !self.detail.is_empty() {
            source.push('\n');
            source.push_str(&self.detail);
        }
        raw_lines_from_source(&source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCell {
    pub tool: String,
    pub reason: String,
    pub input_summary: String,
    pub status: TranscriptItemStatus,
    decision: Option<bool>,
}

impl ApprovalCell {
    fn request(
        tool: &str,
        reason: &str,
        input_summary: &str,
        status: TranscriptItemStatus,
    ) -> Self {
        Self {
            tool: sanitize_label(tool),
            reason: bounded_text(reason.to_string(), MAX_TEXT_CELL_CHARS),
            input_summary: sanitize_label(input_summary),
            status,
            decision: None,
        }
    }

    fn decision(tool: &str, reason: &str, input_summary: &str, allow: bool) -> Self {
        let mut cell = Self::request(
            tool,
            reason,
            input_summary,
            if allow {
                TranscriptItemStatus::Complete
            } else {
                TranscriptItemStatus::Failed
            },
        );
        cell.decision = Some(allow);
        cell
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        self.status = item.status;
        match &item.payload {
            TranscriptPayload::Permission {
                tool,
                reason,
                input_summary,
                ..
            } => {
                self.tool = sanitize_label(tool);
                self.reason = bounded_text(reason.clone(), MAX_TEXT_CELL_CHARS);
                self.input_summary = sanitize_label(input_summary);
                self.decision = None;
            }
            TranscriptPayload::ApprovalDecision {
                allow,
                tool,
                reason,
                input_summary,
            } => {
                self.tool = sanitize_label(tool);
                self.reason = bounded_text(reason.clone(), MAX_TEXT_CELL_CHARS);
                self.input_summary = sanitize_label(input_summary);
                self.decision = Some(*allow);
            }
            _ => {}
        }
    }

    fn title(&self) -> &'static str {
        match self.decision {
            Some(true) => "Approved",
            Some(false) => "Denied",
            None => "Approval requested",
        }
    }
}

impl HistoryCell for ApprovalCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let prefix = match self.decision {
            Some(true) => "✓ ",
            Some(false) => "✗ ",
            None => "? ",
        };
        let prefix_style = match self.decision {
            Some(false) => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default().add_modifier(Modifier::DIM),
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(self.title(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(
                self.tool.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])];
        if !self.input_summary.is_empty() {
            lines.push(Line::from(Span::styled(
                self.input_summary.clone(),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        if !self.reason.is_empty() {
            lines.extend(text_lines(
                &self.reason,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        render_prefixed(
            &lines,
            Span::styled(prefix, prefix_style),
            Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut source = format!("{} {}", self.title(), self.tool);
        if !self.input_summary.is_empty() {
            source.push('\n');
            source.push_str(&self.input_summary);
        }
        if !self.reason.is_empty() {
            source.push('\n');
            source.push_str(&self.reason);
        }
        raw_lines_from_source(&source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeaderCell {
    pub version: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub directory: String,
    pub yolo_mode: bool,
    pub show_fast_status: bool,
}

impl SessionHeaderCell {
    pub fn new(
        version: impl Into<String>,
        model: impl Into<String>,
        reasoning_effort: Option<impl Into<String>>,
        directory: impl Into<String>,
        yolo_mode: bool,
        show_fast_status: bool,
    ) -> Self {
        Self {
            version: sanitize_label(&version.into()),
            model: sanitize_label(&model.into()),
            reasoning_effort: reasoning_effort.map(|value| sanitize_label(&value.into())),
            directory: bounded_text(directory.into(), MAX_TEXT_CELL_CHARS),
            yolo_mode,
            show_fast_status,
        }
    }

    fn formatted_directory(&self, max_width: Option<usize>) -> String {
        let clean = sanitize(&self.directory);
        match max_width {
            Some(width) => measure::truncate(&clean, width),
            None => clean,
        }
    }
}

impl HistoryCell for SessionHeaderCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(inner_width) = card_inner_width(width, SESSION_HEADER_MAX_INNER_WIDTH) else {
            return Vec::new();
        };

        const DIR_LABEL: &str = "directory:";
        const PERMISSIONS_LABEL: &str = "permissions:";
        let label_width = if self.yolo_mode {
            DIR_LABEL.len().max(PERMISSIONS_LABEL.len())
        } else {
            DIR_LABEL.len()
        };

        let mut model_spans = vec![
            Span::styled(
                format!("{:<label_width$} ", "model:"),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                self.model.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(reasoning) = self
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            model_spans.push(Span::raw(" "));
            model_spans.push(Span::raw(reasoning.to_string()));
        }
        if self.show_fast_status {
            model_spans.push(Span::raw("   "));
            model_spans.push(Span::styled("fast", Style::default().fg(Color::Magenta)));
        }
        model_spans.push(Span::styled(
            "   ",
            Style::default().add_modifier(Modifier::DIM),
        ));
        model_spans.push(Span::styled("/model", Style::default().fg(Color::Cyan)));
        model_spans.push(Span::styled(
            " to change",
            Style::default().add_modifier(Modifier::DIM),
        ));

        let dir_prefix = format!("{DIR_LABEL:<label_width$} ");
        let dir_width = measure::width(&dir_prefix);
        let directory = self.formatted_directory(Some(inner_width.saturating_sub(dir_width)));
        let mut lines = vec![
            Line::from(vec![
                Span::styled(">_ ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled("Pyxis", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled(
                    format!("(v{})", self.version),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]),
            Line::default(),
            Line::from(model_spans),
            Line::from(vec![
                Span::styled(dir_prefix, Style::default().add_modifier(Modifier::DIM)),
                Span::raw(directory),
            ]),
        ];

        if self.yolo_mode {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{PERMISSIONS_LABEL:<label_width$} "),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    "YOLO mode",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        with_border(lines, inner_width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(format!("Pyxis (v{})", self.version)),
            Line::from(format!(
                "model: {}{}",
                self.model,
                self.reasoning_effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!(" {value}"))
                    .unwrap_or_default()
            )),
            Line::from(format!(
                "directory: {}",
                self.formatted_directory(/*max_width*/ None)
            )),
        ];
        if self.yolo_mode {
            lines.push(Line::from("permissions: YOLO mode"));
        }
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    Running,
    Completed,
    Failed,
    Blocked,
    Stopped,
}

impl HookStatus {
    fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    fn as_status_text(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Stopped => "stopped",
        }
    }
}

impl From<TranscriptHookStatus> for HookStatus {
    fn from(status: TranscriptHookStatus) -> Self {
        match status {
            TranscriptHookStatus::Running => Self::Running,
            TranscriptHookStatus::Completed => Self::Completed,
            TranscriptHookStatus::Failed => Self::Failed,
            TranscriptHookStatus::Blocked => Self::Blocked,
            TranscriptHookStatus::Stopped => Self::Stopped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutputKind {
    Warning,
    Stop,
    Feedback,
    Context,
    Error,
}

impl HookOutputKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Warning => "warning: ",
            Self::Stop => "stop: ",
            Self::Feedback => "feedback: ",
            Self::Context => "hook context: ",
            Self::Error => "error: ",
        }
    }
}

impl From<TranscriptHookOutputKind> for HookOutputKind {
    fn from(kind: TranscriptHookOutputKind) -> Self {
        match kind {
            TranscriptHookOutputKind::Warning => Self::Warning,
            TranscriptHookOutputKind::Stop => Self::Stop,
            TranscriptHookOutputKind::Feedback => Self::Feedback,
            TranscriptHookOutputKind::Context => Self::Context,
            TranscriptHookOutputKind::Error => Self::Error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutputEntry {
    pub kind: HookOutputKind,
    pub text: String,
}

impl HookOutputEntry {
    pub fn new(kind: HookOutputKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: bounded_text(text.into(), MAX_TEXT_CELL_CHARS),
        }
    }
}

impl From<&TranscriptHookOutputEntry> for HookOutputEntry {
    fn from(entry: &TranscriptHookOutputEntry) -> Self {
        Self::new(HookOutputKind::from(entry.kind), entry.text.clone())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookCell {
    pub event: String,
    pub status_message: Option<String>,
    pub status: HookStatus,
    pub entries: Vec<HookOutputEntry>,
    animations_enabled: bool,
    start_time: Instant,
}

impl HookCell {
    pub fn new(
        event: impl Into<String>,
        status_message: Option<impl Into<String>>,
        status: HookStatus,
        entries: Vec<HookOutputEntry>,
    ) -> Self {
        Self {
            event: sanitize_label(&event.into()),
            status_message: status_message.map(|value| sanitize_label(&value.into())),
            status,
            entries,
            animations_enabled: true,
            start_time: Instant::now(),
        }
    }

    pub fn running(event: impl Into<String>, status_message: Option<impl Into<String>>) -> Self {
        Self::new(event, status_message, HookStatus::Running, Vec::new())
    }

    pub fn completed(
        event: impl Into<String>,
        status_message: Option<impl Into<String>>,
        status: HookStatus,
        entries: Vec<HookOutputEntry>,
    ) -> Self {
        let mut cell = Self::new(event, status_message, status, entries);
        cell.animations_enabled = false;
        cell
    }

    fn mark_status(&mut self, status: TranscriptItemStatus) {
        self.status = match status {
            TranscriptItemStatus::Pending | TranscriptItemStatus::Running => HookStatus::Running,
            TranscriptItemStatus::Complete => {
                if self.status.is_running() {
                    HookStatus::Completed
                } else {
                    self.status
                }
            }
            TranscriptItemStatus::Failed => HookStatus::Failed,
            TranscriptItemStatus::Cancelled => HookStatus::Stopped,
        };
        if !self.status.is_running() {
            self.animations_enabled = false;
        }
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        if let TranscriptPayload::HookRun {
            event,
            status_message,
            status,
            entries,
        } = &item.payload
        {
            self.event = sanitize_label(event);
            self.status_message = status_message.as_ref().map(|value| sanitize_label(value));
            self.status = HookStatus::from(*status);
            self.entries = entries.iter().map(HookOutputEntry::from).collect();
            self.animations_enabled = self.status.is_running();
        } else {
            self.mark_status(item.status);
        }
    }

    fn completed_bullet(&self) -> Span<'static> {
        match self.status {
            HookStatus::Completed => {
                if self
                    .entries
                    .iter()
                    .any(|entry| entry.kind == HookOutputKind::Warning)
                {
                    Span::styled("•", Style::default().add_modifier(Modifier::BOLD))
                } else {
                    Span::styled(
                        "•",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                }
            }
            HookStatus::Failed | HookStatus::Blocked | HookStatus::Stopped => Span::styled(
                "•",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            HookStatus::Running => Span::raw("•"),
        }
    }

    fn running_header(&self) -> Line<'static> {
        let mut spans = vec![
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                format!("Running {} hook", self.event),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(status_message) = self
            .status_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            spans.push(Span::raw(": "));
            spans.push(Span::styled(
                status_message.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        Line::from(spans)
    }
}

impl HistoryCell for HookCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        if self.status.is_running() {
            return vec![self.running_header()];
        }

        let mut lines = vec![Line::from(vec![
            self.completed_bullet(),
            Span::raw(" "),
            Span::raw(format!(
                "{} hook ({})",
                self.event,
                self.status.as_status_text()
            )),
        ])];

        for entry in &self.entries {
            let clean_entry_text = sanitize(&entry.text);
            let mut output_lines = clean_entry_text.split('\n').collect::<Vec<_>>();
            if output_lines.is_empty() {
                continue;
            }
            let first_line = output_lines.remove(0);
            lines.push(Line::from(format!(
                "  {}{}",
                entry.kind.prefix(),
                first_line
            )));
            for line in output_lines {
                if line.is_empty() {
                    lines.push(Line::default());
                } else {
                    lines.push(Line::from(format!("    {line}")));
                }
            }
        }

        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if self.animations_enabled && self.status.is_running() {
            return Some(self.start_time.elapsed().as_millis() as u64 / 600);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStepStatus {
    Completed,
    InProgress,
    Pending,
}

impl PlanStepStatus {
    fn style(self) -> Style {
        match self {
            Self::Completed => Style::default().add_modifier(Modifier::DIM | Modifier::CROSSED_OUT),
            Self::InProgress => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Self::Pending => Style::default().add_modifier(Modifier::DIM),
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Completed => "✔ ",
            Self::InProgress | Self::Pending => "□ ",
        }
    }
}

impl From<TranscriptPlanStepStatus> for PlanStepStatus {
    fn from(status: TranscriptPlanStepStatus) -> Self {
        match status {
            TranscriptPlanStepStatus::Completed => Self::Completed,
            TranscriptPlanStepStatus::InProgress => Self::InProgress,
            TranscriptPlanStepStatus::Pending => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

impl PlanStep {
    pub fn new(step: impl Into<String>, status: PlanStepStatus) -> Self {
        Self {
            step: bounded_text(step.into(), MAX_TEXT_CELL_CHARS),
            status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdateCell {
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

impl PlanUpdateCell {
    pub fn new(explanation: Option<impl Into<String>>, steps: Vec<PlanStep>) -> Self {
        Self {
            explanation: explanation.map(|text| bounded_text(text.into(), MAX_TEXT_CELL_CHARS)),
            steps,
        }
    }
}

impl HistoryCell for PlanUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut logical = vec![Line::from(Span::styled(
            "Updated Plan",
            Style::default().add_modifier(Modifier::BOLD),
        ))];

        if let Some(explanation) = self
            .explanation
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            logical.extend(text_lines(
                explanation,
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            ));
        }

        if self.steps.is_empty() {
            logical.push(Line::from(Span::styled(
                "(no steps provided)",
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )));
        } else {
            for step in &self.steps {
                logical.push(Line::from(vec![
                    Span::styled(step.status.marker(), step.status.style()),
                    Span::styled(step.step.clone(), step.status.style()),
                ]));
            }
        }

        render_prefixed(
            &logical,
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
            width,
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("Updated Plan")];
        if let Some(explanation) = self
            .explanation
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            lines.extend(raw_lines_from_source(explanation));
        }
        if self.steps.is_empty() {
            lines.push(Line::from("(no steps provided)"));
        } else {
            for step in &self.steps {
                lines.push(Line::from(format!("{:?}: {}", step.status, step.step)));
            }
        }
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchCell {
    pub query: String,
    pub detail: Option<String>,
    pub completed: bool,
    animations_enabled: bool,
    start_time: Instant,
}

impl WebSearchCell {
    pub fn searching(query: impl Into<String>, detail: Option<impl Into<String>>) -> Self {
        Self {
            query: sanitize_label(&query.into()),
            detail: detail.map(|value| sanitize_label(&value.into())),
            completed: false,
            animations_enabled: true,
            start_time: Instant::now(),
        }
    }

    pub fn searched(query: impl Into<String>, detail: Option<impl Into<String>>) -> Self {
        let mut cell = Self::searching(query, detail);
        cell.completed = true;
        cell.animations_enabled = false;
        cell
    }

    fn mark_status(&mut self, status: TranscriptItemStatus) {
        self.completed = !matches!(
            status,
            TranscriptItemStatus::Pending | TranscriptItemStatus::Running
        );
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        self.mark_status(item.status);
        if let TranscriptPayload::WebSearch { query, detail } = &item.payload {
            self.query = sanitize_label(query);
            self.detail = detail.as_ref().map(|value| sanitize_label(value));
        }
    }

    fn header(&self) -> &'static str {
        if self.completed {
            "Searched the web"
        } else {
            "Searching the web"
        }
    }

    fn effective_detail(&self) -> String {
        self.detail
            .clone()
            .filter(|detail| !detail.is_empty())
            .unwrap_or_else(|| self.query.clone())
    }
}

impl HistoryCell for WebSearchCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let detail = self.effective_detail();
        let mut spans = vec![Span::styled(
            self.header(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if !detail.is_empty() {
            let separator = if self.completed { " for " } else { " " };
            spans.push(Span::raw(separator));
            spans.push(Span::raw(detail));
        }

        render_prefixed(
            &[Line::from(spans)],
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("  "),
            width,
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let detail = self.effective_detail();
        if detail.is_empty() {
            vec![Line::from(self.header())]
        } else {
            let separator = if self.completed { " for " } else { " " };
            vec![Line::from(format!("{}{separator}{detail}", self.header()))]
        }
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if self.completed || !self.animations_enabled {
            return None;
        }
        Some((self.start_time.elapsed().as_millis() / 50) as u64)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpInvocation {
    pub server: String,
    pub tool: String,
    pub arguments: Option<Value>,
}

impl McpInvocation {
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        arguments: Option<Value>,
    ) -> Self {
        Self {
            server: sanitize_label(&server.into()),
            tool: sanitize_label(&tool.into()),
            arguments,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCell {
    pub invocation: McpInvocation,
    pub status: TranscriptItemStatus,
    pub output: String,
    pub is_error: bool,
    animations_enabled: bool,
    start_time: Instant,
}

impl McpToolCell {
    pub fn calling(invocation: McpInvocation) -> Self {
        Self {
            invocation,
            status: TranscriptItemStatus::Running,
            output: String::new(),
            is_error: false,
            animations_enabled: true,
            start_time: Instant::now(),
        }
    }

    pub fn called(invocation: McpInvocation, output: impl Into<String>, is_error: bool) -> Self {
        let mut cell = Self::calling(invocation);
        cell.output = bounded_text(output.into(), MAX_EXEC_OUTPUT_SCAN_CHARS);
        cell.is_error = is_error;
        cell.status = if is_error {
            TranscriptItemStatus::Failed
        } else {
            TranscriptItemStatus::Complete
        };
        cell.animations_enabled = false;
        cell
    }

    fn mark_status(&mut self, status: TranscriptItemStatus) {
        self.status = status;
    }

    fn apply_item(&mut self, item: &TranscriptItem) {
        self.status = item.status;
        match &item.payload {
            TranscriptPayload::McpToolCall {
                server,
                tool,
                arguments,
            } => {
                self.invocation = McpInvocation::new(server, tool, arguments.clone());
                if matches!(
                    item.status,
                    TranscriptItemStatus::Pending | TranscriptItemStatus::Running
                ) {
                    self.output.clear();
                    self.is_error = false;
                }
            }
            TranscriptPayload::McpToolResult { output, is_error } => {
                self.output = bounded_text(output.clone(), MAX_EXEC_OUTPUT_SCAN_CHARS);
                self.is_error = *is_error;
            }
            _ => {}
        }
    }

    fn is_running(&self) -> bool {
        matches!(
            self.status,
            TranscriptItemStatus::Pending | TranscriptItemStatus::Running
        )
    }

    fn header(&self) -> &'static str {
        if self.is_running() { "Running" } else { "Ran" }
    }

    fn output_for_display(&self) -> String {
        if self.output.trim().is_empty() {
            return String::new();
        }
        if self.is_error && !self.output.trim_start().starts_with("Error:") {
            format!("Error: {}", self.output)
        } else {
            self.output.clone()
        }
    }
}

impl HistoryCell for McpToolCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let bullet_style = if self.is_error || self.status == TranscriptItemStatus::Failed {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else if self.status == TranscriptItemStatus::Complete {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };

        let invocation_line = format_mcp_invocation(&self.invocation);
        let header = Line::from(vec![
            Span::styled("• ", bullet_style),
            Span::styled(self.header(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]);
        let inline_invocation = line_visible_width(&header)
            .saturating_add(line_visible_width(&invocation_line))
            <= safe_width_usize(width);

        if inline_invocation {
            let mut spans = header.spans;
            spans.extend(invocation_line.spans);
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::styled("• ", bullet_style),
                Span::styled(self.header(), Style::default().add_modifier(Modifier::BOLD)),
            ]));
            lines.extend(render_prefixed(
                &[invocation_line],
                Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw("    "),
                width,
            ));
        }

        let output = self.output_for_display();
        if !output.is_empty() {
            let detail_lines = bounded_output_lines(&output, TOOL_CALL_MAX_LINES)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(
                        measure::truncate(
                            &line,
                            safe_width_usize(width).max(RAW_TOOL_OUTPUT_WIDTH),
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                })
                .collect::<Vec<_>>();
            let first_prefix = if inline_invocation {
                Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM))
            } else {
                Span::raw("    ")
            };
            lines.extend(render_prefixed(
                &detail_lines,
                first_prefix,
                Span::raw("    "),
                width,
            ));
        }

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(format!(
            "{} {}",
            self.header(),
            plain_line_text(&format_mcp_invocation(&self.invocation))
        ))];
        let output = self.output_for_display();
        if !output.is_empty() {
            lines.extend(raw_lines_from_source(&output));
        }
        lines
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if !self.is_running() || !self.animations_enabled {
            return None;
        }
        Some((self.start_time.elapsed().as_millis() / 50) as u64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInputQuestion {
    pub id: String,
    pub question: String,
    pub is_secret: bool,
}

impl From<&TranscriptUserInputQuestion> for UserInputQuestion {
    fn from(question: &TranscriptUserInputQuestion) -> Self {
        Self {
            id: sanitize_label(&question.id),
            question: bounded_text(question.question.clone(), MAX_TEXT_CELL_CHARS),
            is_secret: question.is_secret,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInputAnswer {
    pub question_id: String,
    pub answers: Vec<String>,
}

impl From<&TranscriptUserInputAnswer> for UserInputAnswer {
    fn from(answer: &TranscriptUserInputAnswer) -> Self {
        Self {
            question_id: sanitize_label(&answer.question_id),
            answers: answer
                .answers
                .iter()
                .map(|answer| bounded_text(answer.clone(), MAX_TEXT_CELL_CHARS))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestUserInputCell {
    pub questions: Vec<UserInputQuestion>,
    pub answers: Vec<UserInputAnswer>,
    pub interrupted: bool,
}

impl RequestUserInputCell {
    pub fn new(
        questions: Vec<UserInputQuestion>,
        answers: Vec<UserInputAnswer>,
        interrupted: bool,
    ) -> Self {
        Self {
            questions,
            answers,
            interrupted,
        }
    }

    fn answer_for(&self, id: &str) -> Option<&UserInputAnswer> {
        self.answers
            .iter()
            .find(|answer| answer.question_id == id && !answer.answers.is_empty())
    }

    fn answered_count(&self) -> usize {
        self.questions
            .iter()
            .filter(|question| self.answer_for(&question.id).is_some())
            .count()
    }

    fn split_answer(answer: &UserInputAnswer) -> (Vec<String>, Option<String>) {
        let mut options = Vec::new();
        let mut note = None;
        for entry in &answer.answers {
            if let Some(value) = entry.strip_prefix("user_note: ") {
                note = Some(value.to_string());
            } else {
                options.push(entry.clone());
            }
        }
        (options, note)
    }
}

impl HistoryCell for RequestUserInputCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let answered = self.answered_count();
        let total = self.questions.len();
        let mut header = vec![
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("Questions", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {answered}/{total} answered"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ];
        if self.interrupted {
            header.push(Span::styled(
                " (interrupted)",
                Style::default().fg(Color::Cyan),
            ));
        }

        let mut lines = vec![Line::from(header)];
        for question in &self.questions {
            let answer = self.answer_for(&question.id);
            let missing = answer.is_none();
            let mut question_lines = text_lines(&question.question, Style::default());
            if missing && let Some(last) = question_lines.last_mut() {
                last.spans.push(Span::styled(
                    " (unanswered)",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            lines.extend(render_prefixed(
                &question_lines,
                Span::raw("  • "),
                Span::raw("    "),
                width,
            ));

            let Some(answer) = answer else {
                continue;
            };
            if question.is_secret {
                lines.extend(render_prefixed(
                    &[Line::from(Span::styled(
                        "••••••",
                        Style::default().fg(Color::Cyan),
                    ))],
                    Span::styled("    answer: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled("            ", Style::default().add_modifier(Modifier::DIM)),
                    width,
                ));
                continue;
            }

            let (options, note) = Self::split_answer(answer);
            for option in options {
                lines.extend(render_prefixed(
                    &[Line::from(Span::styled(
                        option,
                        Style::default().fg(Color::Cyan),
                    ))],
                    Span::styled("    answer: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled("            ", Style::default().add_modifier(Modifier::DIM)),
                    width,
                ));
            }
            if let Some(note) = note {
                lines.extend(render_prefixed(
                    &[Line::from(Span::styled(
                        note,
                        Style::default().fg(Color::Cyan),
                    ))],
                    Span::styled("    note: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled("          ", Style::default().add_modifier(Modifier::DIM)),
                    width,
                ));
            }
        }

        let unanswered = total.saturating_sub(answered);
        if self.interrupted && unanswered > 0 {
            lines.extend(render_prefixed(
                &[Line::from(Span::styled(
                    format!("interrupted with {unanswered} unanswered"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ))],
                Span::styled("  ↳ ", Style::default().fg(Color::Cyan)),
                Span::styled("    ", Style::default().add_modifier(Modifier::DIM)),
                width,
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let answered = self.answered_count();
        let total = self.questions.len();
        let mut lines = vec![Line::from(format!("Questions {answered}/{total} answered"))];
        if self.interrupted {
            lines.push(Line::from("(interrupted)"));
        }
        for question in &self.questions {
            lines.push(Line::from(question.question.clone()));
            let Some(answer) = self.answer_for(&question.id) else {
                lines.push(Line::from("(unanswered)"));
                continue;
            };
            if question.is_secret {
                lines.push(Line::from("answer: ******"));
                continue;
            }
            let (options, note) = Self::split_answer(answer);
            for option in options {
                lines.push(Line::from(format!("answer: {option}")));
            }
            if let Some(note) = note {
                lines.push(Line::from(format!("note: {note}")));
            }
        }
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalMessageSeparatorCell {
    pub elapsed_seconds: Option<u64>,
    pub metrics: Vec<String>,
}

impl FinalMessageSeparatorCell {
    pub fn new(elapsed_seconds: Option<u64>, metrics: Vec<String>) -> Self {
        Self {
            elapsed_seconds,
            metrics: metrics
                .into_iter()
                .map(|metric| sanitize_label(&metric))
                .filter(|metric| !metric.is_empty())
                .collect(),
        }
    }

    fn label_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(seconds) = self.elapsed_seconds.filter(|seconds| *seconds > 60) {
            parts.push(format!(
                "Worked for {}",
                crate::spinner::fmt_duration(Duration::from_secs(seconds))
            ));
        }
        parts.extend(self.metrics.iter().cloned());
        parts
    }
}

impl HistoryCell for FinalMessageSeparatorCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = safe_width_usize(width);
        let parts = self.label_parts();
        if parts.is_empty() {
            return vec![Line::from(Span::styled(
                "─".repeat(width),
                Style::default().add_modifier(Modifier::DIM),
            ))];
        }

        let label = format!("─ {} ─", parts.join(" • "));
        let label = measure::truncate(&label, width);
        let label_width = measure::width(&label);
        vec![Line::from(vec![
            Span::styled(label, Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                "─".repeat(width.saturating_sub(label_width)),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let parts = self.label_parts();
        if parts.is_empty() {
            Vec::new()
        } else {
            vec![Line::from(parts.join(" • "))]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchChangeKind {
    Added,
    Deleted,
    Edited,
}

impl PatchChangeKind {
    fn verb(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Edited => "Edited",
        }
    }
}

impl From<TranscriptPatchChangeKind> for PatchChangeKind {
    fn from(kind: TranscriptPatchChangeKind) -> Self {
        match kind {
            TranscriptPatchChangeKind::Added => Self::Added,
            TranscriptPatchChangeKind::Deleted => Self::Deleted,
            TranscriptPatchChangeKind::Edited => Self::Edited,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFileChange {
    pub path: String,
    pub move_path: Option<String>,
    pub kind: PatchChangeKind,
    pub added: usize,
    pub removed: usize,
    pub diff: Option<crate::diff::Diff>,
}

impl PatchFileChange {
    pub fn new(
        path: impl Into<String>,
        kind: PatchChangeKind,
        added: usize,
        removed: usize,
        diff: Option<crate::diff::Diff>,
    ) -> Self {
        Self {
            path: sanitize_label(&path.into()),
            move_path: None,
            kind,
            added,
            removed,
            diff,
        }
    }

    pub fn with_move_path(mut self, move_path: impl Into<String>) -> Self {
        self.move_path = Some(sanitize_label(&move_path.into()));
        self
    }

    fn path_label(&self) -> String {
        match self.move_path.as_deref() {
            Some(move_path) if !move_path.is_empty() => format!("{} → {move_path}", self.path),
            _ => self.path.clone(),
        }
    }
}

impl From<&TranscriptPatchFileChange> for PatchFileChange {
    fn from(change: &TranscriptPatchFileChange) -> Self {
        let mut out = PatchFileChange::new(
            change.path.clone(),
            PatchChangeKind::from(change.kind),
            change.added,
            change.removed,
            None,
        );
        out.move_path = change
            .move_path
            .as_ref()
            .map(|move_path| sanitize_label(move_path));
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSummaryCell {
    pub changes: Vec<PatchFileChange>,
}

impl PatchSummaryCell {
    pub fn new(mut changes: Vec<PatchFileChange>) -> Self {
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        Self { changes }
    }

    fn totals(&self) -> (usize, usize) {
        self.changes.iter().fold((0usize, 0usize), |acc, change| {
            (
                acc.0.saturating_add(change.added),
                acc.1.saturating_add(change.removed),
            )
        })
    }

    fn line_count_spans(added: usize, removed: usize) -> Vec<Span<'static>> {
        vec![
            Span::raw("("),
            Span::styled(format!("+{added}"), Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)),
            Span::raw(")"),
        ]
    }

    fn header_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "• ",
            Style::default().add_modifier(Modifier::DIM),
        )];
        match self.changes.as_slice() {
            [change] => {
                spans.push(Span::styled(
                    change.kind.verb(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::raw(change.path_label()));
                spans.push(Span::raw(" "));
                spans.extend(Self::line_count_spans(change.added, change.removed));
            }
            changes => {
                let (added, removed) = self.totals();
                let noun = if changes.len() == 1 { "file" } else { "files" };
                spans.push(Span::styled(
                    "Edited",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(format!(" {} {noun} ", changes.len())));
                spans.extend(Self::line_count_spans(added, removed));
            }
        }
        Line::from(spans)
    }

    fn file_header_line(change: &PatchFileChange) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "  └ ",
            Style::default().add_modifier(Modifier::DIM),
        )];
        spans.push(Span::raw(change.path_label()));
        spans.push(Span::raw(" "));
        spans.extend(Self::line_count_spans(change.added, change.removed));
        Line::from(spans)
    }

    fn push_diff_lines(
        out: &mut Vec<Line<'static>>,
        diff: &crate::diff::Diff,
        theme: &Theme,
        width: u16,
    ) {
        for line in render_file_diff(diff, theme, width.saturating_sub(4)) {
            let mut spans = vec![Span::raw("    ")];
            spans.extend(line.spans);
            out.push(Line::from(spans));
        }
    }
}

impl HistoryCell for PatchSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = Theme::new(true);
        let mut lines = vec![self.header_line()];
        let skip_single_header = self.changes.len() == 1;

        for (idx, change) in self.changes.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            if !skip_single_header {
                lines.push(Self::file_header_line(change));
            }
            if let Some(diff) = &change.diff {
                Self::push_diff_lines(&mut lines, diff, &theme, width);
            }
        }

        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(RAW_TOOL_OUTPUT_WIDTH as u16))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchApplyFailureCell {
    pub stderr: String,
}

impl PatchApplyFailureCell {
    pub fn new(stderr: impl Into<String>) -> Self {
        Self {
            stderr: bounded_text(stderr.into(), MAX_EXEC_OUTPUT_SCAN_CHARS),
        }
    }
}

impl HistoryCell for PatchApplyFailureCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled("✘ ", Style::default().fg(Color::Magenta)),
            Span::styled(
                "Failed to apply patch",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ])];
        if !self.stderr.trim().is_empty() {
            let stderr_lines = bounded_output_lines(&self.stderr, TOOL_CALL_MAX_LINES)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(
                        line,
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                })
                .collect::<Vec<_>>();
            lines.extend(render_prefixed(
                &stderr_lines,
                Span::styled("  └ ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw("    "),
                width,
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("Failed to apply patch")];
        if !self.stderr.trim().is_empty() {
            lines.extend(raw_lines_from_source(&self.stderr));
        }
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialNoticeKind {
    Info,
    Warning,
    Error,
    Deprecation,
    SafetyAccess,
}

impl From<TranscriptNoticeKind> for SpecialNoticeKind {
    fn from(kind: TranscriptNoticeKind) -> Self {
        match kind {
            TranscriptNoticeKind::Info => Self::Info,
            TranscriptNoticeKind::Warning => Self::Warning,
            TranscriptNoticeKind::Error => Self::Error,
            TranscriptNoticeKind::Deprecation => Self::Deprecation,
            TranscriptNoticeKind::SafetyAccess => Self::SafetyAccess,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialNoticeLink {
    pub label: String,
    pub url: String,
}

impl From<&TranscriptNoticeLink> for SpecialNoticeLink {
    fn from(link: &TranscriptNoticeLink) -> Self {
        Self {
            label: sanitize_label(&link.label),
            url: sanitize_label(&link.url),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialNoticeCell {
    pub kind: SpecialNoticeKind,
    pub title: String,
    pub body: Option<String>,
    pub hint: Option<String>,
    pub links: Vec<SpecialNoticeLink>,
}

impl SpecialNoticeCell {
    pub fn new(
        kind: SpecialNoticeKind,
        title: impl Into<String>,
        body: Option<impl Into<String>>,
        hint: Option<impl Into<String>>,
        links: Vec<SpecialNoticeLink>,
    ) -> Self {
        Self {
            kind,
            title: bounded_text(title.into(), MAX_TEXT_CELL_CHARS),
            body: body.map(|value| bounded_text(value.into(), MAX_TEXT_CELL_CHARS)),
            hint: hint.map(|value| bounded_text(value.into(), MAX_TEXT_CELL_CHARS)),
            links,
        }
    }

    fn title_line(&self) -> Line<'static> {
        match self.kind {
            SpecialNoticeKind::Info => {
                let mut spans = vec![
                    Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(self.title.clone()),
                ];
                if let Some(hint) = &self.hint {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        hint.clone(),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                Line::from(spans)
            }
            SpecialNoticeKind::Warning => Line::from(vec![
                Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
                Span::styled(self.title.clone(), Style::default().fg(Color::Yellow)),
            ]),
            SpecialNoticeKind::Error => Line::from(Span::styled(
                format!("■ {}", self.title),
                Style::default().fg(Color::Red),
            )),
            SpecialNoticeKind::Deprecation => Line::from(vec![
                Span::styled("⚠ ", Style::default().fg(Color::Red)),
                Span::styled(self.title.clone(), Style::default().fg(Color::Red)),
            ]),
            SpecialNoticeKind::SafetyAccess => Line::from(vec![
                Span::styled("ⓘ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    self.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
        }
    }
}

impl HistoryCell for SpecialNoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![self.title_line()];
        if let Some(body) = self.body.as_deref().filter(|body| !body.trim().is_empty()) {
            lines.extend(render_prefixed(
                &text_lines(body, Style::default().add_modifier(Modifier::DIM)),
                Span::raw("  "),
                Span::raw("  "),
                width,
            ));
        }
        for link in &self.links {
            lines.extend(render_prefixed(
                &[Line::from(vec![
                    Span::styled(
                        format!("{}: ", link.label),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        link.url.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ])],
                Span::raw("  "),
                Span::raw("  "),
                width,
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(self.title.clone())];
        if let Some(body) = self.body.as_deref().filter(|body| !body.trim().is_empty()) {
            lines.extend(raw_lines_from_source(body));
        }
        if let Some(hint) = self.hint.as_deref().filter(|hint| !hint.trim().is_empty()) {
            lines.push(Line::from(hint.to_string()));
        }
        for link in &self.links {
            lines.push(Line::from(format!("{}: {}", link.label, link.url)));
        }
        lines
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        annotate_web_urls(self.display_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
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

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut source = format!("{} {}", self.title, self.summary);
        if let Some(diff) = &self.diff {
            for row in &diff.rows {
                source.push('\n');
                match row {
                    crate::diff::Row::Add { segs, .. } => {
                        source.push('+');
                        source.push(' ');
                        source.push_str(
                            &segs.iter().map(|seg| seg.text.as_str()).collect::<String>(),
                        );
                    }
                    crate::diff::Row::Remove { segs, .. } => {
                        source.push('-');
                        source.push(' ');
                        source.push_str(
                            &segs.iter().map(|seg| seg.text.as_str()).collect::<String>(),
                        );
                    }
                    crate::diff::Row::Context { text, .. } => {
                        source.push(' ');
                        source.push_str(text);
                    }
                    crate::diff::Row::Gap => source.push_str("..."),
                    crate::diff::Row::Truncated(hidden) => {
                        source.push_str(&format!("... +{hidden} lines"));
                    }
                }
            }
        }
        raw_lines_from_source(&source)
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

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (idx, cell) in self.cells.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            out.extend(cell.raw_lines());
        }
        out
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (idx, cell) in self.cells.iter().enumerate() {
            if idx > 0 {
                out.push(Line::default());
            }
            out.extend(cell.transcript_lines(width));
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
    pending_insert_needs_leading_separator: bool,
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
            pending_insert_needs_leading_separator: false,
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
        self.display_lines_for_mode(width, HistoryRenderMode::Rich)
    }

    pub fn display_lines_for_mode(
        &self,
        width: u16,
        mode: HistoryRenderMode,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.transcript_cells {
            extend_surface_lines(&mut lines, cell.display_lines_for_mode(width, mode));
        }
        for active in &self.active_tools {
            extend_surface_lines(&mut lines, active.cell.display_lines_for_mode(width, mode));
        }
        if let Some(active) = &self.active_cell {
            extend_surface_lines(&mut lines, active.cell.display_lines_for_mode(width, mode));
        }
        lines
    }

    pub fn active_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.active_display_lines_for_mode(width, HistoryRenderMode::Rich)
    }

    pub fn active_display_lines_for_mode(
        &self,
        width: u16,
        mode: HistoryRenderMode,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for active in &self.active_tools {
            extend_surface_lines(&mut lines, active.cell.display_lines_for_mode(width, mode));
        }
        if let Some(active) = &self.active_cell {
            extend_surface_lines(&mut lines, active.cell.display_lines_for_mode(width, mode));
        }
        lines
    }

    pub fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.transcript_cells {
            extend_surface_lines(&mut lines, cell.transcript_lines(width));
        }
        for active in &self.active_tools {
            extend_surface_lines(&mut lines, active.cell.transcript_lines(width));
        }
        if let Some(active) = &self.active_cell {
            extend_surface_lines(&mut lines, active.cell.transcript_lines(width));
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
        if self.pending_insert_needs_leading_separator {
            lines.push(Line::default());
        }
        for cell in self.pending_insert_cells.drain(..) {
            extend_surface_lines(&mut lines, cell.display_lines(width));
        }
        self.pending_insert_needs_leading_separator = false;
        match mode {
            InsertHistoryMode::Legacy => Some(PendingHistoryInsert::legacy_lines(lines)),
            InsertHistoryMode::InlineScrollback => {
                Some(PendingHistoryInsert::inline_scrollback_lines(lines))
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
        let has_prior_transcript = !self.transcript_cells.is_empty();
        if self.pending_insert_cells.is_empty() && has_prior_transcript {
            self.pending_insert_needs_leading_separator = true;
        }
        self.pending_insert_cells.push(cell.clone());
        self.transcript_cells.push(cell);
    }
}

fn extend_surface_lines(out: &mut Vec<Line<'static>>, mut lines: Vec<Line<'static>>) {
    if lines.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(Line::default());
    }
    out.append(&mut lines);
}

pub fn cells_from_messages(messages: &[Message]) -> Vec<HistoryCellKind> {
    let mut cells = Vec::new();
    let mut pending_exec_calls = HashMap::<ToolCallId, PendingExecReplay>::new();
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
                            if let Some(mapping) = tool_exec_mapping_from_tool(name, input) {
                                cells.push(HistoryCellKind::Exec(ExecCell::command_with_id(
                                    Some(TranscriptItemId::derived("exec", id)),
                                    &mapping.command,
                                    TranscriptExecSource::Agent,
                                    TranscriptItemStatus::Complete,
                                )));
                                let index = cells.len().saturating_sub(1);
                                pending_exec_calls.insert(
                                    id.clone(),
                                    PendingExecReplay {
                                        index,
                                        strip_read_line_numbers: mapping.strip_read_line_numbers,
                                    },
                                );
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
                        if let Some(pending) = pending_exec_calls.get(tool_use_id).copied()
                            && let Some(HistoryCellKind::Exec(cell)) = cells.get_mut(pending.index)
                        {
                            let content = if pending.strip_read_line_numbers && !*is_error {
                                strip_numbered_read_output(content)
                            } else {
                                content.clone()
                            };
                            let item = TranscriptItem::new(
                                Some(TranscriptItemId::derived("exec", tool_use_id)),
                                TranscriptRole::Assistant,
                                TranscriptItemKind::ExecCommand,
                                status,
                                TranscriptPayload::ExecOutput {
                                    content,
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
        TranscriptPayload::PlanUpdate { explanation, steps } => {
            Some(HistoryCellKind::PlanUpdate(PlanUpdateCell::new(
                explanation.clone(),
                steps
                    .iter()
                    .map(|step| PlanStep::new(step.step.clone(), PlanStepStatus::from(step.status)))
                    .collect(),
            )))
        }
        TranscriptPayload::WebSearch { query, detail } => {
            let mut cell = WebSearchCell::searching(query.clone(), detail.clone());
            cell.mark_status(item.status);
            Some(HistoryCellKind::WebSearch(cell))
        }
        TranscriptPayload::McpToolCall {
            server,
            tool,
            arguments,
        } => {
            let mut cell =
                McpToolCell::calling(McpInvocation::new(server, tool, arguments.clone()));
            cell.mark_status(item.status);
            Some(HistoryCellKind::McpTool(cell))
        }
        TranscriptPayload::McpToolResult { output, is_error } => {
            Some(HistoryCellKind::McpTool(McpToolCell::called(
                McpInvocation::new("(unknown server)", "(unknown tool)", None),
                output,
                *is_error,
            )))
        }
        TranscriptPayload::SessionHeader {
            version,
            model,
            reasoning_effort,
            directory,
            yolo_mode,
            show_fast_status,
        } => Some(HistoryCellKind::SessionHeader(SessionHeaderCell::new(
            version,
            model,
            reasoning_effort.clone(),
            directory,
            *yolo_mode,
            *show_fast_status,
        ))),
        TranscriptPayload::UserInputResult {
            questions,
            answers,
            interrupted,
        } => Some(HistoryCellKind::RequestUserInput(
            RequestUserInputCell::new(
                questions.iter().map(UserInputQuestion::from).collect(),
                answers.iter().map(UserInputAnswer::from).collect(),
                *interrupted,
            ),
        )),
        TranscriptPayload::PatchSummary { changes } => Some(HistoryCellKind::PatchSummary(
            PatchSummaryCell::new(changes.iter().map(PatchFileChange::from).collect()),
        )),
        TranscriptPayload::PatchApplyFailure { stderr } => Some(
            HistoryCellKind::PatchApplyFailure(PatchApplyFailureCell::new(stderr)),
        ),
        TranscriptPayload::FinalSeparator {
            elapsed_seconds,
            metrics,
        } => Some(HistoryCellKind::FinalSeparator(
            FinalMessageSeparatorCell::new(*elapsed_seconds, metrics.clone()),
        )),
        TranscriptPayload::SpecialNotice {
            kind,
            title,
            body,
            hint,
            links,
        } => Some(HistoryCellKind::SpecialNotice(SpecialNoticeCell::new(
            SpecialNoticeKind::from(*kind),
            title,
            body.clone(),
            hint.clone(),
            links.iter().map(SpecialNoticeLink::from).collect(),
        ))),
        TranscriptPayload::HookRun {
            event,
            status_message,
            status,
            entries,
        } => Some(HistoryCellKind::Hook(HookCell::new(
            event,
            status_message.clone(),
            HookStatus::from(*status),
            entries.iter().map(HookOutputEntry::from).collect(),
        ))),
        TranscriptPayload::Permission {
            tool,
            reason,
            input_summary,
            ..
        } => Some(HistoryCellKind::Approval(ApprovalCell::request(
            tool,
            reason,
            input_summary,
            item.status,
        ))),
        TranscriptPayload::ApprovalDecision {
            allow,
            tool,
            reason,
            input_summary,
        } => Some(HistoryCellKind::Approval(ApprovalCell::decision(
            tool,
            reason,
            input_summary,
            *allow,
        ))),
        TranscriptPayload::Notice { message } => {
            Some(HistoryCellKind::Notice(NoticeCell::new(message)))
        }
        TranscriptPayload::Error { message } => {
            Some(HistoryCellKind::Error(ErrorCell::new(message)))
        }
    }
}

fn line_visible_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| measure::width(span.content.as_ref()))
        .sum()
}

fn plain_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn card_inner_width(width: u16, max_inner_width: usize) -> Option<usize> {
    if width < 4 {
        return None;
    }
    Some((width.saturating_sub(4) as usize).min(max_inner_width))
}

fn with_border(lines: Vec<Line<'static>>, forced_inner_width: usize) -> Vec<Line<'static>> {
    let max_line_width = lines.iter().map(line_visible_width).max().unwrap_or(0);
    let content_width = forced_inner_width.max(max_line_width);
    let border_inner_width = content_width + 2;
    let border_style = Style::default().add_modifier(Modifier::DIM);
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(border_inner_width)),
        border_style,
    )));

    for line in lines {
        let used_width = line_visible_width(&line);
        let mut spans = Vec::with_capacity(line.spans.len() + 4);
        spans.push(Span::styled("│ ", border_style));
        spans.extend(line.spans);
        if used_width < content_width {
            spans.push(Span::styled(
                " ".repeat(content_width - used_width),
                border_style,
            ));
        }
        spans.push(Span::styled(" │", border_style));
        out.push(Line::from(spans));
    }

    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(border_inner_width)),
        border_style,
    )));
    out
}

fn format_mcp_invocation(invocation: &McpInvocation) -> Line<'static> {
    let args = invocation
        .arguments
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled(invocation.server.clone(), Style::default().fg(Color::Cyan)),
        Span::raw("."),
        Span::styled(invocation.tool.clone(), Style::default().fg(Color::Cyan)),
        Span::raw("("),
        Span::styled(args, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(")"),
    ])
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
    let tokens = wrap_tokens(spans);
    if tokens.is_empty() {
        return vec![Vec::new()];
    }

    let mut out = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0usize;
    for token in tokens {
        let token_width = wrap_units_width(&token);
        if is_url_like_token(&token) {
            if !line.is_empty() && line_width.saturating_add(token_width) > width {
                out.push(rebuild_spans(&line));
                line.clear();
                line_width = 0;
            }
            line_width = line_width.saturating_add(token_width);
            line.extend(token);
            continue;
        }

        if token_width > width {
            if !line.is_empty() {
                out.push(rebuild_spans(&line));
                line.clear();
                line_width = 0;
            }
            let mut split = wrap_units_hard(&token, width);
            if let Some(last) = split.pop() {
                out.extend(split.into_iter().map(|units| rebuild_spans(&units)));
                line_width = wrap_units_width(&last);
                line = last;
            }
            continue;
        }

        if !line.is_empty() && line_width.saturating_add(token_width) > width {
            out.push(rebuild_spans(&line));
            line.clear();
            line_width = 0;
            if token
                .iter()
                .all(|unit| unit.text.chars().all(char::is_whitespace))
            {
                continue;
            }
        }
        line_width = line_width.saturating_add(token_width);
        line.extend(token);
    }
    if !line.is_empty() {
        out.push(rebuild_spans(&line));
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WrapUnit {
    text: String,
    style: Style,
    width: usize,
}

fn wrap_tokens(spans: &[Span<'static>]) -> Vec<Vec<WrapUnit>> {
    let mut tokens: Vec<Vec<WrapUnit>> = Vec::new();
    let mut current = Vec::new();
    let mut current_whitespace = None;
    for span in spans {
        for grapheme in span.content.as_ref().graphemes(true) {
            let is_whitespace = grapheme.chars().all(char::is_whitespace);
            if current_whitespace.is_some_and(|known| known != is_whitespace) {
                tokens.push(std::mem::take(&mut current));
            }
            current_whitespace = Some(is_whitespace);
            current.push(WrapUnit {
                text: grapheme.to_string(),
                style: span.style,
                width: measure::width(grapheme),
            });
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn wrap_units_hard(units: &[WrapUnit], width: usize) -> Vec<Vec<WrapUnit>> {
    let mut out = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0usize;
    for unit in units {
        if !line.is_empty() && line_width.saturating_add(unit.width) > width {
            out.push(std::mem::take(&mut line));
            line_width = 0;
        }
        line_width = line_width.saturating_add(unit.width);
        line.push(unit.clone());
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn wrap_units_width(units: &[WrapUnit]) -> usize {
    units.iter().map(|unit| unit.width).sum()
}

fn is_url_like_token(units: &[WrapUnit]) -> bool {
    let text = units
        .iter()
        .map(|unit| unit.text.as_str())
        .collect::<String>();
    let text = text.trim_matches(|ch: char| matches!(ch, '(' | ')' | '[' | ']' | '{' | '}'));
    text.starts_with("http://") || text.starts_with("https://") || text.starts_with("file://")
}

fn rebuild_spans(units: &[WrapUnit]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut buffer = String::new();
    for unit in units {
        if current_style != Some(unit.style) {
            if let Some(style) = current_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buffer), style));
            }
            current_style = Some(unit.style);
        }
        buffer.push_str(&unit.text);
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
    let mut spans = vec![Span::styled(
        format!("{title} "),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    spans.extend(command_spans(first));
    lines.push(Line::from(spans));
    lines.extend(command_lines.map(|line| Line::from(Span::raw(line.to_string()))));
    lines
}

fn command_spans(command: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut first_token = true;
    for token in command.split_inclusive(char::is_whitespace) {
        let (body, trailing_space) = split_trailing_space(token);
        if !body.is_empty() {
            let style = if first_token {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if body.starts_with('-') {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            };
            spans.push(Span::styled(body.to_string(), style));
            first_token = false;
        }
        if !trailing_space.is_empty() {
            spans.push(Span::raw(trailing_space.to_string()));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(command.to_string()));
    }
    spans
}

fn split_trailing_space(token: &str) -> (&str, &str) {
    let split = token
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    token.split_at(split)
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

fn bounded_output_lines(output: &str, line_limit: usize) -> Vec<String> {
    let clean = bound_exec_output_buffer(output);
    let lines = compact_output_preview_lines(
        clean
            .lines()
            .map(str::trim_end)
            .map(str::to_string)
            .collect::<Vec<_>>(),
    );
    let total = lines.len();
    if total <= line_limit {
        return lines;
    }

    let head_lines = line_limit / 2;
    let tail_lines = line_limit.saturating_sub(head_lines + 1);
    let mut out = Vec::new();
    out.extend(lines.iter().take(head_lines).cloned());
    out.push(format!(
        "… +{} lines (ctrl + t to view transcript)",
        total.saturating_sub(head_lines + tail_lines)
    ));
    out.extend(lines.iter().skip(total.saturating_sub(tail_lines)).cloned());
    out
}

fn compact_output_preview_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn exec_output_body_style(is_error: bool) -> Style {
    if is_error {
        Style::default()
            .fg(Color::Rgb(0xd0, 0x6a, 0x6a))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(0xb8, 0xb8, 0xbf))
    }
}

fn exec_output_chrome_style() -> Style {
    Style::default().fg(Color::Rgb(0x78, 0x78, 0x84))
}

fn is_output_hint_line(line: &str) -> bool {
    line.starts_with('…') || line.starts_with("[lines ") || line.starts_with("[file truncated")
}

fn truncate_output_screen_lines(lines: Vec<Line<'static>>, max_lines: usize) -> Vec<Line<'static>> {
    let total = lines.len();
    if total <= max_lines || max_lines == 0 {
        return lines;
    }

    if max_lines == 1 {
        return vec![output_screen_ellipsis_line(total)];
    }

    let head_lines = max_lines / 2;
    let tail_lines = max_lines.saturating_sub(head_lines + 1);
    let mut out = Vec::new();
    out.extend(lines.iter().take(head_lines).cloned());
    out.push(output_screen_ellipsis_line(
        total.saturating_sub(head_lines + tail_lines),
    ));
    out.extend(lines.iter().skip(total.saturating_sub(tail_lines)).cloned());
    out
}

fn output_screen_ellipsis_line(omitted: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled("    ", exec_output_chrome_style()),
        Span::styled(
            format!("… +{omitted} lines (ctrl + t to view transcript)"),
            exec_output_chrome_style(),
        ),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingExecReplay {
    index: usize,
    strip_read_line_numbers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolExecMapping {
    command: String,
    strip_read_line_numbers: bool,
}

fn tool_exec_mapping_from_tool(name: &str, input: &Value) -> Option<ToolExecMapping> {
    match name {
        "bash" => input
            .get("command")
            .and_then(Value::as_str)
            .map(|command| ToolExecMapping {
                command: command.to_string(),
                strip_read_line_numbers: false,
            }),
        "read" => input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| ToolExecMapping {
                command: format!("Get-Content -Raw {}", display_shell_arg(path)),
                strip_read_line_numbers: true,
            }),
        "glob" => input.get("pattern").and_then(Value::as_str).map(|pattern| {
            let mut parts = vec!["Get-ChildItem".to_string(), "-Recurse".to_string()];
            if let Some(path) = input.get("path").and_then(Value::as_str)
                && !path.trim().is_empty()
                && path.trim() != "."
            {
                parts.push(display_shell_arg(path));
            }
            parts.push("-Filter".to_string());
            parts.push(display_shell_arg(pattern));
            ToolExecMapping {
                command: parts.join(" "),
                strip_read_line_numbers: false,
            }
        }),
        "grep" => input.get("pattern").and_then(Value::as_str).map(|pattern| {
            let mut parts = vec!["rg".to_string(), display_shell_arg(pattern)];
            if let Some(glob) = input.get("glob").and_then(Value::as_str)
                && !glob.trim().is_empty()
            {
                parts.push("-g".to_string());
                parts.push(display_shell_arg(glob));
            }
            if let Some(path) = input.get("path").and_then(Value::as_str)
                && !path.trim().is_empty()
            {
                parts.push(display_shell_arg(path));
            }
            ToolExecMapping {
                command: parts.join(" "),
                strip_read_line_numbers: false,
            }
        }),
        _ => None,
    }
}

fn display_shell_arg(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "\"\"".to_string();
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | '(' | ')' | '[' | ']'))
    {
        format!("\"{}\"", trimmed.replace('"', "\\\""))
    } else {
        trimmed.to_string()
    }
}

fn strip_numbered_read_output(content: &str) -> String {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some((prefix, body)) = line.split_once('\t')
            && !prefix.trim().is_empty()
            && prefix.trim().chars().all(|ch| ch.is_ascii_digit())
        {
            out.push(body.to_string());
        } else {
            out.push(line.to_string());
        }
    }
    if content.ends_with('\n') {
        out.push(String::new());
    }
    out.join("\n")
}

fn is_tool_item(item: &TranscriptItem) -> bool {
    matches!(
        &item.payload,
        TranscriptPayload::ToolCall { .. }
            | TranscriptPayload::ToolResult { .. }
            | TranscriptPayload::McpToolCall { .. }
            | TranscriptPayload::McpToolResult { .. }
    ) || matches!(
        item.kind,
        TranscriptItemKind::ToolCall
            | TranscriptItemKind::ToolResult
            | TranscriptItemKind::McpToolCall
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
        assert!(text.iter().any(|line| line.contains("checking")));
        assert!(cells.desired_height(0) >= 1);
    }

    #[test]
    fn user_cell_renders_in_background_block() {
        let cell = UserCell::new("hello world");
        let lines = cell.display_lines(40);
        let flat = flatten(&lines);

        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0], "");
        assert!(flat[1].starts_with("› hello world"));
        assert_eq!(flat[2], "");
        assert_eq!(lines[0].style.bg, Some(user_message_bg()));
        assert_eq!(lines[1].style.bg, Some(user_message_bg()));
        assert_eq!(lines[2].style.bg, Some(user_message_bg()));
    }

    #[test]
    fn user_cell_trims_trailing_blank_message_lines() {
        let cell = UserCell::new("line one\n\n   \n\t \n");
        let rendered = flatten(&cell.display_lines(80));

        assert!(rendered.iter().any(|line| line.contains("line one")));
        assert_eq!(
            rendered
                .iter()
                .rev()
                .take_while(|line| line.trim().is_empty())
                .count(),
            1
        );
    }

    #[test]
    fn reasoning_cell_hides_markdown_header_in_display() {
        let cell = ReasoningCell::new(
            "**Running tests and reviewing files**\n\nI will inspect the repo and run cargo test.",
        );

        let text = flatten(&cell.display_lines(80)).join("\n");

        assert!(text.contains("I will inspect the repo"));
        assert!(!text.contains("Running tests and reviewing files"));
        assert!(!text.contains("**"));
        assert!(!text.contains("reasoning"));
    }

    #[test]
    fn reasoning_cell_uses_header_when_summary_is_missing() {
        let cell = ReasoningCell::new("**Reading project context**");

        let text = flatten(&cell.display_lines(80)).join("\n");

        assert!(text.contains("Reading project context"));
        assert!(!text.contains("**"));
    }

    #[test]
    fn reasoning_cell_preserves_raw_transcript_source() {
        let source = "**Planning**\n\nDetailed reasoning.";
        let cell = ReasoningCell::new(source);

        let raw = flatten(&cell.raw_lines()).join("\n");

        assert_eq!(raw, source);
    }

    #[test]
    fn prefixed_wrap_keeps_url_tokens_intact() {
        let url = "https://example.com/really/long/path";
        let lines = render_prefixed(
            &[Line::from(format!("see {url} now"))],
            Span::raw("• "),
            Span::raw("  "),
            18,
        );
        let flat = flatten(&lines);

        assert!(
            flat.iter().any(|line| line.contains(url)),
            "url token should remain intact: {flat:?}"
        );
    }

    #[test]
    fn chat_surface_exposes_raw_mode_and_transcript_lines() {
        let mut surface = ChatSurface::new();
        surface.apply_update(text_update(
            TranscriptLifecycle::Completed,
            Some(TranscriptItemId::local("assistant", 1)),
            "**bold**",
            TranscriptItemStatus::Complete,
        ));
        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(TranscriptItemId::local("exec", 2)),
            "echo ok",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(TranscriptItemId::local("exec", 2)),
            "ok",
            false,
            TranscriptItemStatus::Complete,
        ));

        let raw = flatten(&surface.display_lines_for_mode(80, HistoryRenderMode::Raw));
        let transcript = flatten(&surface.transcript_lines(80));

        assert!(raw.iter().any(|line| line == "**bold**"));
        assert!(transcript.iter().any(|line| line == "$ echo ok"));
        assert!(transcript.iter().any(|line| line == "succeeded"));
    }

    #[test]
    fn codex_operational_cells_expose_plain_raw_lines() {
        let cells = [
            HistoryCellKind::SessionHeader(SessionHeaderCell::new(
                "0.1.0",
                "gpt-5-codex",
                Some("high"),
                "C:\\dev\\pyxis",
                true,
                true,
            )),
            HistoryCellKind::PlanUpdate(PlanUpdateCell::new(
                Some("Port rendering parity"),
                vec![
                    PlanStep::new("map Codex cells", PlanStepStatus::Completed),
                    PlanStep::new("wire Pyxis snapshots", PlanStepStatus::InProgress),
                ],
            )),
            HistoryCellKind::WebSearch(WebSearchCell::searched(
                "codex tui history_cell",
                Some("plans.rs"),
            )),
            HistoryCellKind::McpTool(McpToolCell::called(
                McpInvocation::new(
                    "github",
                    "list_issues",
                    Some(serde_json::json!({ "state": "open" })),
                ),
                "issue #1",
                false,
            )),
            HistoryCellKind::RequestUserInput(RequestUserInputCell::new(
                vec![UserInputQuestion {
                    id: "scope".into(),
                    question: "Which scope?".into(),
                    is_secret: false,
                }],
                vec![UserInputAnswer {
                    question_id: "scope".into(),
                    answers: vec!["All".into()],
                }],
                false,
            )),
            HistoryCellKind::FinalSeparator(FinalMessageSeparatorCell::new(
                Some(75),
                vec!["Local tools: 2 calls (3s)".to_string()],
            )),
            HistoryCellKind::PatchSummary(PatchSummaryCell::new(vec![PatchFileChange::new(
                "src/lib.rs",
                PatchChangeKind::Edited,
                2,
                1,
                None,
            )])),
            HistoryCellKind::PatchApplyFailure(PatchApplyFailureCell::new("anchor missing")),
            HistoryCellKind::SpecialNotice(SpecialNoticeCell::new(
                SpecialNoticeKind::SafetyAccess,
                "This content can't be shown",
                Some("Eligible researchers can apply for Trusted Access."),
                None::<String>,
                vec![SpecialNoticeLink {
                    label: "Learn more".into(),
                    url: "https://help.openai.com/en/articles/20001326".into(),
                }],
            )),
            HistoryCellKind::Hook(HookCell::completed(
                "PostToolUse",
                None::<String>,
                HookStatus::Completed,
                vec![HookOutputEntry::new(
                    HookOutputKind::Warning,
                    "format changed",
                )],
            )),
        ];

        let raw = cells
            .iter()
            .flat_map(|cell| flatten(&cell.display_lines_for_mode(80, HistoryRenderMode::Raw)))
            .collect::<Vec<_>>();

        assert!(raw.iter().any(|line| line == "Pyxis (v0.1.0)"));
        assert!(raw.iter().any(|line| line == "Updated Plan"));
        assert!(raw.iter().any(|line| line.contains("Searched the web")));
        assert!(
            raw.iter()
                .any(|line| { line.contains("Ran github.list_issues({\"state\":\"open\"})") })
        );
        assert!(raw.iter().any(|line| line == "Questions 1/1 answered"));
        assert!(raw.iter().any(|line| line.contains("Worked for 1m 15s")));
        assert!(raw.iter().any(|line| line.contains("Edited src/lib.rs")));
        assert!(raw.iter().any(|line| line == "Failed to apply patch"));
        assert!(raw.iter().any(|line| line == "This content can't be shown"));
        assert!(
            raw.iter()
                .any(|line| line.contains("PostToolUse hook (completed)"))
        );
        assert!(raw.iter().any(|line| line == "  warning: format changed"));
    }

    #[test]
    fn chat_surface_maps_codex_operational_payloads() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("session", 0)),
                TranscriptRole::System,
                TranscriptItemKind::SessionHeader,
                TranscriptItemStatus::Complete,
                TranscriptPayload::SessionHeader {
                    version: "0.1.0".into(),
                    model: "gpt-5-codex".into(),
                    reasoning_effort: Some("high".into()),
                    directory: "C:\\dev\\pyxis".into(),
                    yolo_mode: false,
                    show_fast_status: true,
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("plan", 1)),
                TranscriptRole::Assistant,
                TranscriptItemKind::PlanUpdate,
                TranscriptItemStatus::Complete,
                TranscriptPayload::PlanUpdate {
                    explanation: Some("Port cells".into()),
                    steps: vec![crate::app_event::TranscriptPlanStep {
                        step: "wire mapper".into(),
                        status: crate::app_event::TranscriptPlanStepStatus::Completed,
                    }],
                },
            ),
        });
        let web_id = TranscriptItemId::local("web", 2);
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(web_id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::WebSearch,
                TranscriptItemStatus::Running,
                TranscriptPayload::WebSearch {
                    query: "codex tui".into(),
                    detail: Some("history_cell".into()),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(web_id),
                TranscriptRole::Assistant,
                TranscriptItemKind::WebSearch,
                TranscriptItemStatus::Complete,
                TranscriptPayload::WebSearch {
                    query: "codex tui".into(),
                    detail: Some("history_cell".into()),
                },
            ),
        });
        let mcp_id = TranscriptItemId::local("mcp", 3);
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(mcp_id.clone()),
                TranscriptRole::Assistant,
                TranscriptItemKind::McpToolCall,
                TranscriptItemStatus::Running,
                TranscriptPayload::McpToolCall {
                    server: "github".into(),
                    tool: "list_issues".into(),
                    arguments: Some(serde_json::json!({ "state": "open" })),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(mcp_id),
                TranscriptRole::Tool,
                TranscriptItemKind::McpToolCall,
                TranscriptItemStatus::Complete,
                TranscriptPayload::McpToolResult {
                    output: "issue #1".into(),
                    is_error: false,
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("patch", 4)),
                TranscriptRole::Assistant,
                TranscriptItemKind::PatchSummary,
                TranscriptItemStatus::Complete,
                TranscriptPayload::PatchSummary {
                    changes: vec![crate::app_event::TranscriptPatchFileChange {
                        path: "src/lib.rs".into(),
                        move_path: None,
                        kind: crate::app_event::TranscriptPatchChangeKind::Edited,
                        added: 2,
                        removed: 1,
                    }],
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("questions", 5)),
                TranscriptRole::Assistant,
                TranscriptItemKind::UserInputRequest,
                TranscriptItemStatus::Complete,
                TranscriptPayload::UserInputResult {
                    questions: vec![crate::app_event::TranscriptUserInputQuestion {
                        id: "scope".into(),
                        question: "Which scope?".into(),
                        is_secret: false,
                    }],
                    answers: vec![crate::app_event::TranscriptUserInputAnswer {
                        question_id: "scope".into(),
                        answers: vec!["All".into()],
                    }],
                    interrupted: false,
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("sep", 6)),
                TranscriptRole::Assistant,
                TranscriptItemKind::FinalSeparator,
                TranscriptItemStatus::Complete,
                TranscriptPayload::FinalSeparator {
                    elapsed_seconds: Some(75),
                    metrics: vec!["Local tools: 2 calls (3s)".into()],
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("notice", 7)),
                TranscriptRole::System,
                TranscriptItemKind::SpecialNotice,
                TranscriptItemStatus::Complete,
                TranscriptPayload::SpecialNotice {
                    kind: crate::app_event::TranscriptNoticeKind::SafetyAccess,
                    title: "This content can't be shown".into(),
                    body: Some("Eligible researchers can apply for Trusted Access.".into()),
                    hint: None,
                    links: vec![crate::app_event::TranscriptNoticeLink {
                        label: "Learn more".into(),
                        url: "https://help.openai.com/en/articles/20001326".into(),
                    }],
                },
            ),
        });
        let hook_id = TranscriptItemId::local("hook", 8);
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(hook_id.clone()),
                TranscriptRole::System,
                TranscriptItemKind::HookRun,
                TranscriptItemStatus::Running,
                TranscriptPayload::HookRun {
                    event: "PreToolUse".into(),
                    status_message: Some("checking command".into()),
                    status: crate::app_event::TranscriptHookStatus::Running,
                    entries: Vec::new(),
                },
            ),
        });
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(hook_id),
                TranscriptRole::System,
                TranscriptItemKind::HookRun,
                TranscriptItemStatus::Complete,
                TranscriptPayload::HookRun {
                    event: "PreToolUse".into(),
                    status_message: None,
                    status: crate::app_event::TranscriptHookStatus::Completed,
                    entries: vec![crate::app_event::TranscriptHookOutputEntry {
                        kind: crate::app_event::TranscriptHookOutputKind::Feedback,
                        text: "allowed".into(),
                    }],
                },
            ),
        });

        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Pyxis"));
        assert!(text.contains("Updated Plan"));
        assert!(text.contains("Searched the web for history_cell"));
        assert!(text.contains("Ran github.list_issues({\"state\":\"open\"})"));
        assert!(text.contains("Edited src/lib.rs"));
        assert!(text.contains("Questions 1/1 answered"));
        assert!(text.contains("Worked for 1m 15s"));
        assert!(text.contains("This content can't be shown"));
        assert!(text.contains("PreToolUse hook (completed)"));
        assert!(text.contains("feedback: allowed"));
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
                "session header",
                HistoryCellKind::SessionHeader(SessionHeaderCell::new(
                    "0.1.0",
                    "gpt-5-codex",
                    Some("high"),
                    "C:\\dev\\pyxis",
                    true,
                    true,
                )),
            ),
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
                "plan update",
                HistoryCellKind::PlanUpdate(PlanUpdateCell::new(
                    Some("Port Codex rendering"),
                    vec![
                        PlanStep::new("explore history_cell", PlanStepStatus::Completed),
                        PlanStep::new("adapt missing cells", PlanStepStatus::InProgress),
                        PlanStep::new("run gates", PlanStepStatus::Pending),
                    ],
                )),
            ),
            cell_snapshot(
                "web search active",
                HistoryCellKind::WebSearch(WebSearchCell::searching(
                    "codex tui history_cell",
                    Some("mcp.rs"),
                )),
            ),
            cell_snapshot(
                "web search complete",
                HistoryCellKind::WebSearch(WebSearchCell::searched(
                    "codex tui history_cell",
                    Some("plans.rs"),
                )),
            ),
            cell_snapshot(
                "mcp calling",
                HistoryCellKind::McpTool(McpToolCell::calling(McpInvocation::new(
                    "github",
                    "list_pull_requests",
                    Some(serde_json::json!({ "owner": "openai" })),
                ))),
            ),
            cell_snapshot(
                "mcp called",
                HistoryCellKind::McpTool(McpToolCell::called(
                    McpInvocation::new(
                        "github",
                        "list_issues",
                        Some(serde_json::json!({ "state": "open" })),
                    ),
                    "issue #1\nissue #2",
                    false,
                )),
            ),
            cell_snapshot(
                "request user input",
                HistoryCellKind::RequestUserInput(RequestUserInputCell::new(
                    vec![
                        UserInputQuestion {
                            id: "scope".into(),
                            question: "Which scope?".into(),
                            is_secret: false,
                        },
                        UserInputQuestion {
                            id: "token".into(),
                            question: "Secret token?".into(),
                            is_secret: true,
                        },
                    ],
                    vec![
                        UserInputAnswer {
                            question_id: "scope".into(),
                            answers: vec!["All".into(), "user_note: include snapshots".into()],
                        },
                        UserInputAnswer {
                            question_id: "token".into(),
                            answers: vec!["secret".into()],
                        },
                    ],
                    false,
                )),
            ),
            cell_snapshot(
                "final separator",
                HistoryCellKind::FinalSeparator(FinalMessageSeparatorCell::new(
                    Some(75),
                    vec!["Local tools: 2 calls (3s)".to_string()],
                )),
            ),
            cell_snapshot(
                "patch summary",
                HistoryCellKind::PatchSummary(PatchSummaryCell::new(vec![
                    PatchFileChange::new(
                        "src/lib.rs",
                        PatchChangeKind::Edited,
                        2,
                        1,
                        crate::diff::from_tool(
                            "edit",
                            &serde_json::json!({
                                "old_string": "old()\n",
                                "new_string": "new()\n",
                            }),
                        ),
                    ),
                    PatchFileChange::new("src/new.rs", PatchChangeKind::Added, 1, 0, None),
                ])),
            ),
            cell_snapshot(
                "patch failure",
                HistoryCellKind::PatchApplyFailure(PatchApplyFailureCell::new(
                    "could not find anchor",
                )),
            ),
            cell_snapshot(
                "special notice",
                HistoryCellKind::SpecialNotice(SpecialNoticeCell::new(
                    SpecialNoticeKind::SafetyAccess,
                    "This content can't be shown",
                    Some("Eligible researchers can apply for Trusted Access."),
                    None::<String>,
                    vec![SpecialNoticeLink {
                        label: "Learn more".into(),
                        url: "https://help.openai.com/en/articles/20001326".into(),
                    }],
                )),
            ),
            cell_snapshot(
                "hook running",
                HistoryCellKind::Hook(HookCell::running("PreToolUse", Some("checking command"))),
            ),
            cell_snapshot(
                "hook completed",
                HistoryCellKind::Hook(HookCell::completed(
                    "PostToolUse",
                    None::<String>,
                    HookStatus::Completed,
                    vec![HookOutputEntry::new(
                        HookOutputKind::Feedback,
                        "allowed\nwith context",
                    )],
                )),
            ),
            cell_snapshot(
                "queue input resize",
                HistoryCellKind::Composite(CompositeCell::new(vec![
                    HistoryCellKind::User(UserCell::new("queued")),
                    HistoryCellKind::Notice(NoticeCell::new("Message ajouté à la file d'attente.")),
                ])),
            ),
        ];

        assert_eq!(snapshots.len(), 33);
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
    fn pending_insert_keeps_breathing_between_incremental_cells() {
        let mut surface = ChatSurface::new();
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("user", 1)),
                TranscriptRole::User,
                TranscriptItemKind::UserMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Text {
                    delta: "first block".into(),
                },
            ),
        });

        let first = surface
            .drain_pending_insert(80, InsertHistoryMode::InlineScrollback)
            .expect("first insert");
        assert_eq!(first.lines[0].as_str(), "");
        assert!(
            first
                .lines
                .iter()
                .any(|line| line.as_str().contains("first block"))
        );

        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(TranscriptItemId::local("assistant", 2)),
                TranscriptRole::Assistant,
                TranscriptItemKind::AssistantMessage,
                TranscriptItemStatus::Complete,
                TranscriptPayload::Text {
                    delta: "second block".into(),
                },
            ),
        });

        let second = surface
            .drain_pending_insert(80, InsertHistoryMode::InlineScrollback)
            .expect("second insert");
        assert_eq!(second.lines[0].as_str(), "");
        assert!(
            second
                .lines
                .iter()
                .any(|line| line.as_str().contains("second block"))
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
    fn exec_output_preview_drops_blank_lines() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "call-1");
        let output = "# Pyxis\n\n\n## Install\n\n[lines 1-4 of 4; offset=5 to continue]";

        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "Get-Content -Raw README.md",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(id),
            output,
            false,
            TranscriptItemStatus::Complete,
        ));

        let lines = flatten(&surface.display_lines(80));

        assert!(lines.iter().any(|line| line.contains("# Pyxis")));
        assert!(lines.iter().any(|line| line.contains("## Install")));
        assert!(!lines.iter().any(|line| line.trim().is_empty()));
        assert!(!lines.iter().any(|line| line.trim() == "└"));
    }

    #[test]
    fn exec_output_truncates_after_wrapping() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("exec", "call-1");
        let output = "this-is-a-long-output-token-without-breaks".repeat(8);

        surface.apply_update(exec_command_update(
            TranscriptLifecycle::Started,
            Some(id.clone()),
            "cat huge.log",
            TranscriptItemStatus::Running,
        ));
        surface.apply_update(exec_output_update(
            TranscriptLifecycle::Completed,
            Some(id),
            &output,
            false,
            TranscriptItemStatus::Complete,
        ));

        let lines = surface.display_lines(24);
        let text = flatten(&lines).join("\n");

        assert!(
            lines.len() <= TOOL_CALL_MAX_LINES + 1,
            "wrapped output should stay capped: {text}"
        );
        assert!(text.contains("ctrl + t to view transcript"));
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
        let search = text.find("Ran rg shimmer").expect("search cell");
        let ran = text.find("Ran echo after").expect("run cell");
        assert!(search < ran);
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
    fn exploratory_exec_commands_render_as_individual_runs() {
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

        assert_eq!(surface.transcript_cells().len(), 3);
        let active = flatten(&surface.active_display_lines(100)).join("\n");
        assert!(active.is_empty());
        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Ran rg shimmer_spans"));
        assert!(text.contains("Ran cat shimmer.rs"));
        assert!(text.contains("Ran cat status_indicator_widget.rs"));

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

        assert_eq!(surface.transcript_cells().len(), 3);
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
            "long text that should wrap differently depending on width",
            true,
        );

        let wide = cell.display_lines(80).len();
        let narrow = cell.display_lines(14).len();

        assert!(narrow > wide);
        assert_eq!(
            cell.stream_view().raw_source,
            "long text that should wrap differently depending on width"
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
        assert!(text.contains("Ran Get-Content -Raw a.rs"));
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
    fn replay_strips_padded_read_line_numbers() {
        let messages = vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "a.rs" }),
            }]),
            Message::tool_result("c1", "     1\tfn main() {}\n     2\t", false),
        ];

        let surface = ChatSurface::from_messages(&messages);
        let text = flatten(&surface.display_lines(80)).join("\n");

        assert!(text.contains("fn main() {}"));
        assert!(!text.contains("     1"));
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
    fn permission_request_finalizes_as_approval_decision_cell() {
        let mut surface = ChatSurface::new();
        let id = TranscriptItemId::derived("permission", "call-1");
        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Started,
            item: TranscriptItem::new(
                Some(id.clone()),
                TranscriptRole::System,
                TranscriptItemKind::PermissionRequest,
                TranscriptItemStatus::Pending,
                TranscriptPayload::Permission {
                    tool: "bash".into(),
                    reason: "writes workspace files".into(),
                    taint_forced: false,
                    input_summary: "cargo test".into(),
                    mode: "ask".into(),
                    input: serde_json::json!({ "command": "cargo test" }),
                },
            ),
        });

        let active = flatten(&surface.active_display_lines(100)).join("\n");
        assert!(active.contains("Approval requested"));
        assert!(active.contains("cargo test"));

        surface.apply_update(TranscriptUpdate {
            lifecycle: TranscriptLifecycle::Completed,
            item: TranscriptItem::new(
                Some(id),
                TranscriptRole::System,
                TranscriptItemKind::ApprovalDecision,
                TranscriptItemStatus::Failed,
                TranscriptPayload::ApprovalDecision {
                    allow: false,
                    tool: "bash".into(),
                    reason: "writes workspace files".into(),
                    input_summary: "cargo test".into(),
                },
            ),
        });

        assert!(surface.active_cell().is_none());
        let text = flatten(&surface.display_lines(100)).join("\n");
        assert!(text.contains("Denied bash"));
        assert!(text.contains("writes workspace files"));
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
