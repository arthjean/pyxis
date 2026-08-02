//! Projection of a thread into ordered items (US-016 AC2, US-018 AC1).
//!
//! Two surfaces read the same shapes: the live stream (`item/started`,
//! `item/completed`) and the paginated history (`thread/items/list`). One
//! projector serves both, so a client never has to reconcile two vocabularies.
//!
//! **Item identity.** `item_<n>` names a DURABLE item, `n` being its rank in
//! the projection of the thread log. The live projector is seeded with the
//! number of items the log already held, so a live identifier is the one the
//! same item will carry once re-read from disk. Two rules keep that promise:
//! an assistant message abandoned before commit gives its number back
//! (`StreamReset`), and an artifact the log will never hold (an engine error
//! report) is named in the separate `error_<n>` namespace instead of consuming
//! a durable number. The divergence from Codex, whose identifiers are minted
//! per turn, is recorded in `docs/app-server/README.md`.

use agent_core::error::AgentError;
use agent_core::event::AgentEvent;
use agent_core::message::{ContentBlock, Message, Role};
use agent_core::tools::ToolResultStatus;

use crate::protocol::{ItemStatus, ProviderErrorCategoryView, ThreadItem};

/// Tools whose call is a command execution rather than a generic tool call.
const COMMAND_TOOLS: &[&str] = &["bash", "exec_command", "write_stdin"];
/// Tools whose call changes files.
const FILE_CHANGE_TOOLS: &[&str] = &["write", "edit", "apply_patch"];

/// What the live projection produced for one runtime event.
#[derive(Debug, Clone, PartialEq)]
pub enum Projected {
    /// A new item, or (same id, twice) the signal that what was buffered for a
    /// streaming item must be discarded.
    Started(ThreadItem),
    Completed(ThreadItem),
    AssistantDelta {
        item_id: String,
        delta: String,
    },
    CommandDelta {
        item_id: String,
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ResponseMetadata(Box<agent_core::provider::ResponseMetadata>),
    ResponseItem {
        phase: agent_core::provider::ResponseItemPhase,
        output_index: Option<u64>,
        item: Box<agent_core::provider::ResponseItem>,
    },
    ProviderExtension(agent_core::provider::ProviderExtension),
}

/// An item still waiting for its terminal projection.
#[derive(Debug, Clone)]
struct OpenItem {
    item: ThreadItem,
    /// Output accumulated by the deltas, folded into the terminal item when the
    /// tool produced no final content of its own.
    streamed: String,
}

/// Projects one thread. Not `Sync`-shared: one projector per open thread, owned
/// by the connection that owns the thread.
#[derive(Debug, Default)]
pub struct Projector {
    next_ordinal: u64,
    /// Numbering of what the log will never hold, kept out of `next_ordinal`.
    next_error: u64,
    /// Assistant text being streamed, and the id it was announced under.
    pending_message: Option<(String, String)>,
    /// Items started and not terminal yet, in start order.
    open: Vec<(String, OpenItem)>,
}

impl Projector {
    /// A projector seeded on an already projected history.
    pub fn resumed(item_count: u64) -> Self {
        Self {
            next_ordinal: item_count,
            ..Self::default()
        }
    }

    fn mint(&mut self) -> String {
        let id = format!("item_{}", self.next_ordinal);
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        id
    }

    /// Identifiers of every item still open, in start order.
    pub fn open_item_ids(&self) -> Vec<String> {
        self.open.iter().map(|(id, _)| id.clone()).collect()
    }

    /// The item a call opened, when it is still open. What correlates an
    /// approval and a client-side tool execution to what the client is showing
    /// (US-017 AC1).
    pub fn item_for_call(&self, call_id: &str) -> Option<String> {
        self.open
            .iter()
            .find(|(_, open)| item_call_id(&open.item) == Some(call_id))
            .map(|(id, _)| id.clone())
    }

    /// The item of an accepted client input. Terminal on creation: a user
    /// message has nothing left to happen to it.
    pub fn user_message(&mut self, text: String) -> ThreadItem {
        ThreadItem::UserMessage {
            id: self.mint(),
            text,
        }
    }

    /// Projects one engine event.
    pub fn engine(&mut self, event: &AgentEvent) -> Vec<Projected> {
        match event {
            AgentEvent::Text(chunk) if chunk.is_empty() => Vec::new(),
            AgentEvent::Text(chunk) => {
                let mut out = Vec::new();
                let id = match &mut self.pending_message {
                    Some((id, text)) => {
                        text.push_str(chunk);
                        id.clone()
                    }
                    None => {
                        let id = self.mint();
                        self.pending_message = Some((id.clone(), chunk.clone()));
                        out.push(Projected::Started(ThreadItem::AssistantMessage {
                            id: id.clone(),
                            text: String::new(),
                        }));
                        id
                    }
                };
                out.push(Projected::AssistantDelta {
                    item_id: id,
                    delta: chunk.clone(),
                });
                out
            }
            AgentEvent::Reasoning(chunk) => vec![Projected::ReasoningDelta {
                delta: chunk.clone(),
            }],
            AgentEvent::ResponseMetadata(metadata) => {
                vec![Projected::ResponseMetadata(metadata.clone())]
            }
            AgentEvent::ResponseItem {
                phase,
                output_index,
                item,
            } => vec![Projected::ResponseItem {
                phase: *phase,
                output_index: *output_index,
                item: item.clone(),
            }],
            AgentEvent::ProviderExtension(extension) => {
                vec![Projected::ProviderExtension(extension.clone())]
            }
            AgentEvent::UnmappedResponseItem {
                item_type,
                extension,
            } => vec![Projected::ProviderExtension(
                extension.clone().unwrap_or_else(|| {
                    agent_core::provider::ProviderExtension::from_value(
                        format!("response.item.{item_type}"),
                        serde_json::json!({"item_type": item_type}),
                    )
                }),
            )],
            // The sampling was abandoned before commit: what was streamed is not
            // what the transcript will hold. The item is re-announced empty
            // rather than completed, which keeps its identifier from naming text
            // nobody will ever read again.
            AgentEvent::StreamReset => match &mut self.pending_message {
                Some((id, text)) => {
                    text.clear();
                    vec![Projected::Started(ThreadItem::AssistantMessage {
                        id: id.clone(),
                        text: String::new(),
                    })]
                }
                None => Vec::new(),
            },
            AgentEvent::ToolCall(call) => {
                let mut out = self.commit_message();
                let id = self.mint();
                let item = new_tool_item(&id, call.id.as_str(), &call.name, &call.input);
                self.open.push((
                    id,
                    OpenItem {
                        item: item.clone(),
                        streamed: String::new(),
                    },
                ));
                out.push(Projected::Started(item));
                out
            }
            AgentEvent::ToolOutputDelta(delta) => {
                let Some((id, open)) = self.find_by_call(delta.id.as_str()) else {
                    return Vec::new();
                };
                // The app-server protocol carries text, so the bytes are decoded
                // here, once, at the edge that serializes them.
                let text = delta.chunk_lossy().into_owned();
                open.streamed.push_str(&text);
                match &open.item {
                    ThreadItem::CommandExecution { .. } => vec![Projected::CommandDelta {
                        item_id: id,
                        delta: text,
                    }],
                    // Other families have no streaming contract: their output
                    // reaches the client with `item/completed`.
                    _ => Vec::new(),
                }
            }
            AgentEvent::ToolResult(result) => {
                let Some(position) = self
                    .open
                    .iter()
                    .position(|(_, open)| item_call_id(&open.item) == Some(result.id.as_str()))
                else {
                    return Vec::new();
                };
                let (_, open) = self.open.remove(position);
                vec![Projected::Completed(complete_tool_item(open, result))]
            }
            AgentEvent::EndTurn | AgentEvent::Interrupted(..) => self.commit_message(),
            AgentEvent::Error(error) => {
                let mut out = self.commit_message();
                let id = format!("error_{}", self.next_error);
                self.next_error = self.next_error.saturating_add(1);
                let (provider_category, status, retry_after_ms, request_id, auth_request_id) =
                    match error {
                        AgentError::Provider(failure) => (
                            failure
                                .provider_category
                                .map(ProviderErrorCategoryView::from),
                            failure.status,
                            failure.retry_after_ms,
                            failure.request_id.clone(),
                            failure.auth_request_id.clone(),
                        ),
                        _ => (None, None, None, None, None),
                    };
                out.push(Projected::Completed(ThreadItem::Error {
                    id,
                    message: error.to_string(),
                    provider_category,
                    status,
                    retry_after_ms,
                    request_id,
                    auth_request_id,
                }));
                out
            }
            _ => Vec::new(),
        }
    }

    /// Closes every item still open, once each, with the terminal cause of the
    /// turn (US-017 AC3). Called on a terminal transition, never per event: a
    /// turn that ends leaves no item waiting for a result that will not come.
    pub fn close_open(&mut self, cause: &str) -> Vec<Projected> {
        let mut out = self.commit_message();
        for (_, open) in std::mem::take(&mut self.open) {
            out.push(Projected::Completed(fail_item(open, cause)));
        }
        out
    }

    /// Turns the streamed text into its terminal item. Empty text produces
    /// nothing: an item is what the transcript will hold, and it will hold no
    /// empty assistant message.
    fn commit_message(&mut self) -> Vec<Projected> {
        match self.pending_message.take() {
            Some((id, text)) if !text.is_empty() => {
                vec![Projected::Completed(ThreadItem::AssistantMessage {
                    id,
                    text,
                })]
            }
            // The identifier is given back: nothing was committed under it, so
            // the numbering stays the numbering of what is durable.
            Some((id, _)) => {
                if let Some(previous) = id
                    .strip_prefix("item_")
                    .and_then(|raw| raw.parse::<u64>().ok())
                    && previous.saturating_add(1) == self.next_ordinal
                {
                    self.next_ordinal = previous;
                }
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    fn find_by_call(&mut self, call_id: &str) -> Option<(String, &mut OpenItem)> {
        let (id, open) = self
            .open
            .iter_mut()
            .find(|(_, open)| item_call_id(&open.item) == Some(call_id))?;
        Some((id.clone(), open))
    }
}

/// Projects a durable transcript. Returns the items in persistent order, which
/// is what `thread/items/list` pages over.
pub fn project_messages(messages: &[Message]) -> Vec<ThreadItem> {
    let mut items: Vec<ThreadItem> = Vec::new();
    let mut ordinal = 0_u64;
    // A result is appended by a LATER message than the call it answers, so a
    // call is opened `inProgress` and closed when its result is read.
    for message in messages {
        match message.role {
            Role::System => {}
            Role::User => {
                let text = text_of(&message.content);
                if !text.is_empty() {
                    items.push(ThreadItem::UserMessage {
                        id: mint(&mut ordinal),
                        text,
                    });
                }
            }
            Role::Assistant | Role::Tool => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            items.push(ThreadItem::AssistantMessage {
                                id: mint(&mut ordinal),
                                text: text.clone(),
                            });
                        }
                        ContentBlock::ToolUse {
                            id: call_id,
                            name,
                            input,
                            ..
                        } => {
                            let id = mint(&mut ordinal);
                            items.push(new_tool_item(&id, call_id.as_str(), name, input));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            untrusted,
                            execution,
                            status,
                            ..
                        } => {
                            let Some(item) = items.iter_mut().find(|item| {
                                item_call_id(item) == Some(tool_use_id.as_str())
                                    && item.status() == ItemStatus::InProgress
                            }) else {
                                continue;
                            };
                            apply_result(
                                item,
                                content,
                                *is_error,
                                *untrusted,
                                execution.as_ref().and_then(|exec| exec.exit_code),
                                *status,
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    items
}

fn mint(ordinal: &mut u64) -> String {
    let id = format!("item_{ordinal}");
    *ordinal = ordinal.saturating_add(1);
    id
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn new_tool_item(id: &str, call_id: &str, name: &str, input: &serde_json::Value) -> ThreadItem {
    if COMMAND_TOOLS.contains(&name) {
        return ThreadItem::CommandExecution {
            id: id.to_string(),
            call_id: call_id.to_string(),
            command: command_of(name, input),
            output: String::new(),
            exit_code: None,
            status: ItemStatus::InProgress,
        };
    }
    if FILE_CHANGE_TOOLS.contains(&name) {
        return ThreadItem::FileChange {
            id: id.to_string(),
            call_id: call_id.to_string(),
            paths: paths_of(input),
            output: String::new(),
            status: ItemStatus::InProgress,
        };
    }
    ThreadItem::ToolCall {
        id: id.to_string(),
        call_id: call_id.to_string(),
        tool: name.to_string(),
        input: input.clone(),
        output: String::new(),
        status: ItemStatus::InProgress,
        untrusted: true,
    }
}

/// The command, as the human reads it. A freeform call carries raw text rather
/// than an object, which is why the string case is handled and not assumed away.
fn command_of(name: &str, input: &serde_json::Value) -> String {
    if let Some(text) = input.as_str() {
        return text.to_string();
    }
    for key in ["command", "cmd", "chars"] {
        match input.get(key) {
            Some(serde_json::Value::String(value)) => return value.clone(),
            Some(serde_json::Value::Array(parts)) => {
                return parts
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            _ => {}
        }
    }
    name.to_string()
}

/// Paths a file-changing call names. `apply_patch` carries them inside its
/// patch text, which is not parsed here: the client gets the raw input and the
/// tool's own result, never a guess.
fn paths_of(input: &serde_json::Value) -> Vec<String> {
    ["path", "file_path", "target"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

fn item_call_id(item: &ThreadItem) -> Option<&str> {
    match item {
        ThreadItem::CommandExecution { call_id, .. }
        | ThreadItem::FileChange { call_id, .. }
        | ThreadItem::ToolCall { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    }
}

fn complete_tool_item(open: OpenItem, result: &agent_core::event::ToolResultView) -> ThreadItem {
    let mut item = open.item;
    let content = if result.content.is_empty() {
        open.streamed
    } else {
        result.content.clone()
    };
    apply_result(
        &mut item,
        &content,
        result.is_error,
        result.untrusted,
        result.execution.as_ref().and_then(|exec| exec.exit_code),
        result.status,
    );
    item
}

fn apply_result(
    item: &mut ThreadItem,
    content: &str,
    is_error: bool,
    result_untrusted: bool,
    exit: Option<i32>,
    result_status: Option<ToolResultStatus>,
) {
    let terminal = if is_error
        || matches!(result_status, Some(status) if status != ToolResultStatus::Success)
    {
        ItemStatus::Failed
    } else {
        ItemStatus::Completed
    };
    match item {
        ThreadItem::CommandExecution {
            output,
            exit_code,
            status,
            ..
        } => {
            *output = content.to_string();
            *exit_code = exit.map(i64::from);
            *status = terminal;
        }
        ThreadItem::FileChange { output, status, .. } => {
            *output = content.to_string();
            *status = terminal;
        }
        ThreadItem::ToolCall {
            output,
            status,
            untrusted,
            ..
        } => {
            *output = content.to_string();
            *status = terminal;
            *untrusted = result_untrusted;
        }
        _ => {}
    }
}

/// Closes an item the turn ended under. One terminal projection per item, with
/// the turn's cause as its output, so nothing stays `inProgress` forever.
fn fail_item(open: OpenItem, cause: &str) -> ThreadItem {
    let mut item = open.item;
    let content = if open.streamed.is_empty() {
        cause.to_string()
    } else {
        format!("{}\n{cause}", open.streamed)
    };
    apply_result(
        &mut item,
        &content,
        true,
        true,
        None,
        Some(ToolResultStatus::Cancelled),
    );
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::error::{ProviderFailure, ProviderFailureKind};
    use agent_core::event::{ToolCallView, ToolOutputDeltaView, ToolResultView};
    use agent_core::message::ToolCallId;
    use agent_core::provider::ProviderErrorCategory;

    fn call(id: &str, name: &str, input: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolCall(ToolCallView {
            id: ToolCallId::from(id.to_string()),
            name: name.to_string(),
            input,
            kind: Default::default(),
        })
    }

    fn result(id: &str, content: &str, is_error: bool) -> AgentEvent {
        AgentEvent::ToolResult(ToolResultView {
            id: ToolCallId::from(id.to_string()),
            content: content.to_string(),
            status: Some(if is_error {
                ToolResultStatus::Error
            } else {
                ToolResultStatus::Success
            }),
            structured_content: None,
            is_error,
            error_kind: None,
            untrusted: true,
            duration_ms: None,
            truncation: None,
            execution: None,
        })
    }

    #[test]
    fn a_streamed_message_is_announced_then_completed_under_one_identifier() {
        let mut projector = Projector::default();
        let started = projector.engine(&AgentEvent::Text("Hel".into()));
        assert!(matches!(
            started.as_slice(),
            [
                Projected::Started(ThreadItem::AssistantMessage { id, .. }),
                Projected::AssistantDelta { item_id, .. },
            ] if id == "item_0" && item_id == "item_0"
        ));
        projector.engine(&AgentEvent::Text("lo".into()));
        let ended = projector.engine(&AgentEvent::EndTurn);
        assert!(matches!(
            ended.as_slice(),
            [Projected::Completed(ThreadItem::AssistantMessage { id, text })]
                if id == "item_0" && text == "Hello"
        ));
    }

    /// A sampling abandoned before commit re-announces its item instead of
    /// completing it: the numbering keeps naming what the log will hold.
    #[test]
    fn a_stream_reset_reopens_the_same_identifier() {
        let mut projector = Projector::default();
        projector.engine(&AgentEvent::Text("draft".into()));
        let reset = projector.engine(&AgentEvent::StreamReset);
        assert!(matches!(
            reset.as_slice(),
            [Projected::Started(ThreadItem::AssistantMessage { id, text })]
                if id == "item_0" && text.is_empty()
        ));
        projector.engine(&AgentEvent::Text("final".into()));
        let ended = projector.engine(&AgentEvent::EndTurn);
        assert!(matches!(
            ended.as_slice(),
            [Projected::Completed(ThreadItem::AssistantMessage { id, text })]
                if id == "item_0" && text == "final"
        ));
    }

    #[test]
    fn an_empty_message_consumes_no_identifier() {
        let mut projector = Projector::default();
        projector.engine(&AgentEvent::Text("x".into()));
        projector.engine(&AgentEvent::StreamReset);
        assert!(projector.engine(&AgentEvent::EndTurn).is_empty());
        // The next item takes the identifier the discarded message held.
        let started = projector.engine(&call("c1", "read", serde_json::json!({})));
        assert_eq!(
            started
                .iter()
                .filter_map(|projected| match projected {
                    Projected::Started(item) => Some(item.id().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["item_0".to_string()]
        );
    }

    #[test]
    fn response_metadata_and_response_items_are_never_lost_by_the_projector() {
        let mut projector = Projector::default();
        let metadata = agent_core::provider::ResponseMetadata {
            response_id: Some("resp_1".into()),
            model: Some("gpt-effective".into()),
            service_tier: Some("priority".into()),
            request_id: Some("req_1".into()),
            turn_state: Some("sticky".into()),
            models_etag: Some("etag-1".into()),
            end_turn: Some(false),
            safety: agent_core::provider::SafetyMetadata {
                use_cases: vec!["cyber".into()],
                reasons: vec!["review".into()],
                retry_model: Some("gpt-safe".into()),
            },
            verifications: vec!["trusted_access_for_cyber".into()],
            moderation: Some(agent_core::provider::ProviderExtension::from_value(
                "turn_moderation_metadata",
                serde_json::json!({"flagged": false}),
            )),
            reasoning: agent_core::provider::ReasoningMetadata {
                server_included: Some(true),
                item_id: Some("rs_1".into()),
                status: Some("completed".into()),
            },
        };
        assert!(matches!(
            projector
                .engine(&AgentEvent::ResponseMetadata(Box::new(metadata)))
                .as_slice(),
            [Projected::ResponseMetadata(metadata)] if metadata.response_id.as_deref() == Some("resp_1")
        ));

        let item = agent_core::provider::ResponseItem::from_wire(&serde_json::json!({
            "type": "web_search_call",
            "id": "ws_1",
            "status": "completed",
            "action": {"type": "search", "query": "safe"}
        }))
        .unwrap();
        assert!(matches!(
            projector
                .engine(&AgentEvent::ResponseItem {
                    phase: agent_core::provider::ResponseItemPhase::Done,
                    output_index: Some(2),
                    item: Box::new(item),
                })
                .as_slice(),
            [Projected::ResponseItem { phase, output_index: Some(2), item }]
                if *phase == agent_core::provider::ResponseItemPhase::Done
                    && item.id() == Some("ws_1")
        ));

        let extension = agent_core::provider::ProviderExtension::from_value(
            "response.item.future",
            serde_json::json!({"type": "future", "authorization": "secret"}),
        );
        assert!(matches!(
            projector
                .engine(&AgentEvent::UnmappedResponseItem {
                    item_type: "future".into(),
                    extension: Some(extension),
                })
                .as_slice(),
            [Projected::ProviderExtension(extension)]
                if extension.event_type() == "response.item.future"
                    && extension.was_redacted()
                    && extension.payload()["authorization"] == "[REDACTED]"
        ));
    }

    #[test]
    fn provider_error_diagnostics_survive_the_app_server_projection() {
        let error = AgentEvent::Error(AgentError::Provider(ProviderFailure {
            kind: ProviderFailureKind::Http,
            status: Some(429),
            message: "provider API request failed: RateLimited".into(),
            retry_after_ms: Some(11_054),
            class: None,
            provider_category: Some(ProviderErrorCategory::RateLimited),
            request_id: Some("req_1".into()),
            auth_request_id: Some("auth_req_1".into()),
        }));

        assert!(matches!(
            Projector::default().engine(&error).as_slice(),
            [Projected::Completed(ThreadItem::Error {
                provider_category: Some(ProviderErrorCategoryView::RateLimited),
                status: Some(429),
                retry_after_ms: Some(11_054),
                request_id: Some(request_id),
                auth_request_id: Some(auth_request_id),
                ..
            })] if request_id == "req_1" && auth_request_id == "auth_req_1"
        ));
    }

    #[test]
    fn a_command_call_streams_its_output_and_carries_its_exit_code() {
        let mut projector = Projector::default();
        projector.engine(&call("c1", "bash", serde_json::json!({"command": "ls -l"})));
        let delta = projector.engine(&AgentEvent::ToolOutputDelta(ToolOutputDeltaView {
            id: ToolCallId::from("c1".to_string()),
            stream: agent_core::event::OutputStream::Stdout,
            chunk: b"total 0\n".to_vec(),
        }));
        assert!(matches!(
            delta.as_slice(),
            [Projected::CommandDelta { item_id, delta }]
                if item_id == "item_0" && delta == "total 0\n"
        ));
        let completed = projector.engine(&result("c1", "total 0", false));
        assert!(matches!(
            completed.as_slice(),
            [Projected::Completed(ThreadItem::CommandExecution { command, status, .. })]
                if command == "ls -l" && *status == ItemStatus::Completed
        ));
    }

    #[test]
    fn a_file_change_is_its_own_family() {
        let mut projector = Projector::default();
        let started = projector.engine(&call(
            "c1",
            "edit",
            serde_json::json!({"path": "src/lib.rs", "old_string": "a", "new_string": "b"}),
        ));
        assert!(matches!(
            started.as_slice(),
            [Projected::Started(ThreadItem::FileChange { paths, .. })]
                if paths == &vec!["src/lib.rs".to_string()]
        ));
    }

    /// US-017 AC3: an interruption leaves nothing `inProgress`, and closes each
    /// open item exactly once.
    #[test]
    fn closing_open_items_is_terminal_and_happens_once() {
        let mut projector = Projector::default();
        projector.engine(&call(
            "c1",
            "bash",
            serde_json::json!({"command": "sleep 9"}),
        ));
        projector.engine(&call("c2", "read", serde_json::json!({"path": "a"})));
        let closed = projector.close_open("interrupted");
        assert_eq!(closed.len(), 2);
        assert!(closed.iter().all(|projected| matches!(
            projected,
            Projected::Completed(item) if item.status() == ItemStatus::Failed
        )));
        assert!(projector.close_open("interrupted").is_empty());
        assert!(projector.open_item_ids().is_empty());
    }

    #[test]
    fn a_transcript_projects_in_persistent_order() {
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::ToolUse {
                    id: ToolCallId::from("c1".to_string()),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    format: Default::default(),
                },
            ]),
            Message::tool_result("c1", "a.txt", false),
            Message::assistant_text("done"),
        ];
        let items = project_messages(&messages);
        let ids: Vec<&str> = items.iter().map(ThreadItem::id).collect();
        assert_eq!(ids, vec!["item_0", "item_1", "item_2", "item_3"]);
        assert!(matches!(items[0], ThreadItem::UserMessage { .. }));
        assert!(matches!(
            &items[2],
            ThreadItem::CommandExecution { status, output, .. }
                if *status == ItemStatus::Completed && output == "a.txt"
        ));
        assert!(matches!(&items[3], ThreadItem::AssistantMessage { text, .. } if text == "done"));
    }

    /// The live numbering continues the history it resumed from, so the two
    /// surfaces of one open thread never hand out the same identifier twice.
    #[test]
    fn a_resumed_projection_continues_the_history_numbering() {
        let messages = vec![Message::user("go"), Message::assistant_text("done")];
        let history = project_messages(&messages);
        let mut projector = Projector::resumed(history.len() as u64);
        let started = projector.engine(&call("c1", "read", serde_json::json!({})));
        assert_eq!(
            started
                .iter()
                .filter_map(|projected| match projected {
                    Projected::Started(item) => Some(item.id().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["item_2".to_string()]
        );
    }
}
