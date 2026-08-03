//! The event pump of one open thread (US-017, US-018).
//!
//! One connection = one read loop and one pump. The read loop decodes, checks
//! ownership and drives the runtime; the pump owns everything that must be
//! ordered against the event stream: the item projection, the pending server
//! requests and the current turn. That split is what makes the correlation of
//! an approval deterministic rather than lucky: the pump polls the runtime
//! events BEFORE its own command queue, so the item a call opens is always
//! projected before the approval that call raises is sent out.

use std::collections::HashMap;

use agent_runtime::thread::{RuntimeEvent, RuntimeEventPayload};
use agent_tools::permission::{ApprovalResponse, PermissionRequest};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::items::{Projected, Projector, ToolFamily, declared_paths};
use crate::jsonrpc::{ErrorObject, Outbound, RequestId};
use crate::outbound::Outbox;
use crate::protocol::*;

/// What the pump must serialize against the event stream.
pub(crate) enum PumpCommand {
    Approve {
        request: Box<PermissionRequest>,
        reply: oneshot::Sender<ApprovalResponse>,
    },
    DynamicTool {
        tool: String,
        call_id: String,
        arguments: Value,
        reply: oneshot::Sender<Result<DynamicToolCallResponse, String>>,
    },
    ClientResponse {
        id: RequestId,
        result: Result<Value, ErrorObject>,
    },
}

/// A server request waiting for its answer.
struct PendingRequest {
    method: &'static str,
    turn_id: Option<String>,
    item_id: String,
    answer: PendingAnswer,
}

enum PendingAnswer {
    Approval(oneshot::Sender<ApprovalResponse>),
    DynamicTool(oneshot::Sender<Result<DynamicToolCallResponse, String>>),
}

impl PendingAnswer {
    /// Releases the caller without an answer. Releasing means DENYING: a
    /// request nobody will answer must not leave a tool waiting, and a
    /// permission nobody granted is a permission refused.
    fn fail(self, detail: &str) {
        match self {
            Self::Approval(reply) => {
                let _ = reply.send(ApprovalResponse::DENY_ONCE);
            }
            Self::DynamicTool(reply) => {
                let _ = reply.send(Err(detail.to_string()));
            }
        }
    }
}

/// Everything ordered against the runtime event stream.
pub(crate) struct Pump {
    thread_id: String,
    outbox: Outbox,
    projector: Projector,
    pending: HashMap<RequestId, PendingRequest>,
    next_request_id: i64,
    current_turn: Option<String>,
    /// Inputs accepted for a turn that has not started yet.
    ///
    /// The runtime makes an input durable and announces it BEFORE it commits
    /// the turn to `Running` (`persist_input`, then `start_next_turn`), so an
    /// `InputAccepted` legitimately precedes the `turn/started` of its own
    /// turn. Publishing it straight away would hand the client an item for a
    /// turn it has not been told about; it waits here instead.
    queued_inputs: HashMap<String, Vec<String>>,
}

impl Pump {
    pub(crate) fn new(thread_id: String, outbox: Outbox, projector: Projector) -> Self {
        Self {
            thread_id,
            outbox,
            projector,
            pending: HashMap::new(),
            next_request_id: 1,
            current_turn: None,
            queued_inputs: HashMap::new(),
        }
    }

    pub(crate) async fn run(
        mut self,
        mut events: broadcast::Receiver<RuntimeEvent>,
        mut commands: mpsc::UnboundedReceiver<PumpCommand>,
    ) {
        loop {
            tokio::select! {
                // Runtime events FIRST: the item a call opens must be projected
                // before the approval that call raises is sent to the client.
                biased;
                received = events.recv() => match received {
                    Ok(event) => self.on_event(&event),
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        self.error(self.current_turn.clone(), format!(
                            "{dropped} runtime event(s) dropped from the live stream; \
                             re-read the thread with `thread/items/list`"
                        ));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                command = commands.recv() => match command {
                    Some(command) => self.on_command(command),
                    None => break,
                },
            }
        }
        self.cancel_pending(None);
    }

    fn on_event(&mut self, event: &RuntimeEvent) {
        let turn_id = event.turn_id.map(|id| id.to_string());
        match &event.payload {
            RuntimeEventPayload::InputAccepted { text } => {
                let Some(turn) = turn_id else {
                    return;
                };
                if self.current_turn.as_deref() == Some(turn.as_str()) {
                    self.emit_input(&turn, text);
                } else {
                    self.queued_inputs
                        .entry(turn)
                        .or_default()
                        .push(text.clone());
                }
            }
            RuntimeEventPayload::TurnStateChanged { to, cause, .. } => {
                let Some(turn) = turn_id else {
                    return;
                };
                match to {
                    agent_runtime::TurnState::Running => self.on_turn_running(turn),
                    state if state.is_terminal() => {
                        self.on_turn_terminal(turn, *state, cause.as_deref());
                    }
                    _ => {}
                }
            }
            RuntimeEventPayload::Engine(engine) => {
                let projected = self.projector.engine(engine);
                self.publish(projected, turn_id);
            }
            RuntimeEventPayload::StoreFailed { operation, detail } => {
                self.error(
                    turn_id,
                    format!("thread store failed during {operation}: {detail}"),
                );
            }
            RuntimeEventPayload::ShuttingDown => {
                self.emit(ServerNotification::ThreadClosed(ThreadClosedNotification {
                    thread_id: self.thread_id.clone(),
                    reason: "the runtime is shutting down".to_string(),
                }));
            }
            RuntimeEventPayload::Forked { .. } => {}
        }
    }

    /// A turn that came back from `needs_input` is the same turn: it is
    /// announced once, not once per resumption.
    fn on_turn_running(&mut self, turn: String) {
        let already = self.current_turn.as_deref() == Some(turn.as_str());
        self.current_turn = Some(turn.clone());
        if !already {
            self.emit(ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: self.thread_id.clone(),
                turn_id: turn.clone(),
            }));
        }
        for text in self.queued_inputs.remove(&turn).unwrap_or_default() {
            self.emit_input(&turn, &text);
        }
    }

    fn on_turn_terminal(
        &mut self,
        turn: String,
        state: agent_runtime::TurnState,
        cause: Option<&str>,
    ) {
        let cause_text = cause
            .map(str::to_string)
            .unwrap_or_else(|| format!("turn {}", terminal_label(state)));
        // US-019 AC1: read once, by the classifier the TUI, the headless summary
        // and the durable log all share.
        let failure = agent_runtime::TurnFailure::classify(state, cause);
        // Exactly one terminal projection per open item, before the turn is
        // declared over (US-017 AC3).
        let closed = self.projector.close_open(&cause_text);
        self.publish(closed, Some(turn.clone()));
        self.cancel_pending(Some(&turn));
        self.queued_inputs.remove(&turn);
        self.emit(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                thread_id: self.thread_id.clone(),
                turn_id: turn.clone(),
                status: match state {
                    agent_runtime::TurnState::Completed => TurnStatus::Completed,
                    agent_runtime::TurnState::Interrupted => TurnStatus::Interrupted,
                    _ => TurnStatus::Failed,
                },
                cause: cause.map(str::to_string),
                cause_category: failure.as_ref().map(|failure| failure.category.into()),
                cause_guidance: failure
                    .as_ref()
                    .map(|failure| failure.category.guidance().to_string()),
            },
        ));
        if self.current_turn.as_deref() == Some(turn.as_str()) {
            self.current_turn = None;
        }
    }

    fn emit_input(&mut self, turn: &str, text: &str) {
        let item = self.projector.user_message(text.to_string());
        self.emit(ServerNotification::ItemCompleted(ItemNotification {
            thread_id: self.thread_id.clone(),
            turn_id: Some(turn.to_string()),
            item,
        }));
    }

    fn publish(&self, projected: Vec<Projected>, turn_id: Option<String>) {
        for one in projected {
            self.emit(one.into_notification(&self.thread_id, turn_id.clone()));
        }
    }

    fn on_command(&mut self, command: PumpCommand) {
        match command {
            PumpCommand::Approve { request, reply } => self.ask_approval(*request, reply),
            PumpCommand::DynamicTool {
                tool,
                call_id,
                arguments,
                reply,
            } => self.ask_dynamic_tool(tool, call_id, arguments, reply),
            PumpCommand::ClientResponse { id, result } => self.answer(id, result),
        }
    }

    fn ask_approval(
        &mut self,
        request: PermissionRequest,
        reply: oneshot::Sender<ApprovalResponse>,
    ) {
        let call_id = request.call_id.as_str().to_string();
        let item_id = self.correlated_item(&call_id);
        let correlation = RequestCorrelation {
            thread_id: self.thread_id.clone(),
            turn_id: self.current_turn.clone(),
            item_id: item_id.clone(),
            call_id,
        };
        // The SAME table the item projection read, so the family a client is
        // asked to approve is the family its item is rendered as.
        let server_request = match ToolFamily::of(&request.tool) {
            ToolFamily::Command => {
                ServerRequest::CommandExecutionRequestApproval(CommandExecutionApprovalParams {
                    correlation,
                    command: request.input_summary.clone(),
                    reason: request.reason.clone(),
                    taint_forced: request.taint_forced,
                    mode: request.mode.to_string(),
                    memoizable: request.memoizable,
                    memo_refused: request.memo_refused.clone(),
                })
            }
            ToolFamily::FileChange => {
                ServerRequest::FileChangeRequestApproval(FileChangeApprovalParams {
                    correlation,
                    tool: request.tool.clone(),
                    paths: declared_paths(&request.input),
                    input: request.input.clone(),
                    reason: request.reason.clone(),
                    taint_forced: request.taint_forced,
                    mode: request.mode.to_string(),
                    memoizable: request.memoizable,
                    memo_refused: request.memo_refused.clone(),
                })
            }
            ToolFamily::Other => ServerRequest::ToolRequestApproval(ToolApprovalParams {
                correlation,
                tool: request.tool.clone(),
                input: request.input.clone(),
                summary: request.input_summary.clone(),
                reason: request.reason.clone(),
                taint_forced: request.taint_forced,
                mode: request.mode.to_string(),
                memoizable: request.memoizable,
                memo_refused: request.memo_refused.clone(),
            }),
        };
        self.send_request(server_request, item_id, PendingAnswer::Approval(reply));
    }

    fn ask_dynamic_tool(
        &mut self,
        tool: String,
        call_id: String,
        arguments: Value,
        reply: oneshot::Sender<Result<DynamicToolCallResponse, String>>,
    ) {
        let item_id = self.correlated_item(&call_id);
        let request = ServerRequest::DynamicToolCall(DynamicToolCallParams {
            correlation: RequestCorrelation {
                thread_id: self.thread_id.clone(),
                turn_id: self.current_turn.clone(),
                item_id: item_id.clone(),
                call_id,
            },
            tool,
            arguments,
        });
        self.send_request(request, item_id, PendingAnswer::DynamicTool(reply));
    }

    /// The item a call is showing as, falling back to the call identifier when
    /// the item is already closed: a correlation the client cannot resolve is
    /// still better than none.
    fn correlated_item(&self, call_id: &str) -> String {
        self.projector
            .item_for_call(call_id)
            .unwrap_or_else(|| call_id.to_string())
    }

    fn send_request(&mut self, request: ServerRequest, item_id: String, answer: PendingAnswer) {
        let id = RequestId::Number(self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        let method = request.method_name();
        let sent = self.outbox.send(Outbound::Request {
            id: id.clone(),
            method: method.to_string(),
            params: request.params(),
        });
        if sent.is_err() {
            // The queue is closed: nothing will ever answer, so the caller is
            // released now rather than after a timeout it does not have.
            answer.fail("the client connection is closed");
            return;
        }
        self.pending.insert(
            id,
            PendingRequest {
                method,
                turn_id: self.current_turn.clone(),
                item_id,
                answer,
            },
        );
    }

    /// Routes a client answer. A late, duplicated or unknown answer is REFUSED
    /// and reported: it reopens no item and runs no tool (US-017 AC4).
    fn answer(&mut self, id: RequestId, result: Result<Value, ErrorObject>) {
        let Some(pending) = self.pending.remove(&id) else {
            self.error(
                self.current_turn.clone(),
                format!(
                    "response to request {id} refused: it is unknown, already \
                     answered, or belongs to a turn that ended"
                ),
            );
            return;
        };
        let PendingRequest {
            turn_id,
            item_id,
            answer,
            ..
        } = pending;
        if let Some(refusal) = deliver(answer, result) {
            self.error(turn_id.clone(), refusal);
        }
        self.emit(ServerNotification::ServerRequestResolved(
            ServerRequestResolvedNotification {
                request_id: id.to_string(),
                thread_id: Some(self.thread_id.clone()),
                turn_id,
                item_id: Some(item_id),
                resolution: ServerRequestResolution::Answered,
            },
        ));
    }

    /// Releases every request bound to `turn` (all of them when `None`).
    fn cancel_pending(&mut self, turn: Option<&str>) {
        let doomed: Vec<(RequestId, PendingRequest)> = self
            .pending
            .extract_if(|_, pending| match turn {
                None => true,
                Some(turn) => pending.turn_id.as_deref() == Some(turn),
            })
            .collect();
        for (id, pending) in doomed {
            tracing::debug!(
                target: "pyxis::app_server",
                request_id = %id,
                method = pending.method,
                "server request cancelled"
            );
            pending
                .answer
                .fail("the turn ended before the client answered");
            self.emit(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    request_id: id.to_string(),
                    thread_id: Some(self.thread_id.clone()),
                    turn_id: pending.turn_id,
                    item_id: Some(pending.item_id),
                    resolution: ServerRequestResolution::Cancelled,
                },
            ));
        }
    }

    fn emit(&self, notification: ServerNotification) {
        let _ = self.outbox.send(notification.into());
    }

    /// A failure that belongs to no single request, on this thread.
    fn error(&self, turn_id: Option<String>, message: String) {
        self.emit(ServerNotification::Error(ErrorNotification {
            thread_id: Some(self.thread_id.clone()),
            turn_id,
            message,
        }));
    }
}

/// Hands a client answer to whoever was waiting for it. Returns what must be
/// reported to the client, when the answer could not be honoured.
fn deliver(answer: PendingAnswer, result: Result<Value, ErrorObject>) -> Option<String> {
    match (answer, result) {
        (PendingAnswer::Approval(reply), Ok(value)) => {
            match serde_json::from_value::<ApprovalDecisionResponse>(value) {
                Ok(decision) => {
                    let _ = reply.send(match decision.decision {
                        ApprovalDecision::Approved => ApprovalResponse::ALLOW_ONCE,
                        ApprovalDecision::ApprovedForSession => ApprovalResponse::ALLOW_SESSION,
                        ApprovalDecision::Declined => ApprovalResponse::DENY_ONCE,
                        ApprovalDecision::DeclinedForSession => ApprovalResponse::DENY_SESSION,
                    });
                    None
                }
                // An unreadable decision denies: the fail-closed rule of the
                // whole permission pipeline does not stop at the wire.
                Err(err) => {
                    let _ = reply.send(ApprovalResponse::DENY_ONCE);
                    Some(format!("unreadable approval answer, denied: {err}"))
                }
            }
        }
        (PendingAnswer::Approval(reply), Err(error)) => {
            let _ = reply.send(ApprovalResponse::DENY_ONCE);
            Some(format!(
                "client refused the approval request: {}",
                error.message
            ))
        }
        (PendingAnswer::DynamicTool(reply), Ok(value)) => {
            let _ = reply.send(
                serde_json::from_value::<DynamicToolCallResponse>(value)
                    .map_err(|err| format!("unreadable tool answer: {err}")),
            );
            None
        }
        (PendingAnswer::DynamicTool(reply), Err(error)) => {
            let _ = reply.send(Err(error.message));
            None
        }
    }
}

fn terminal_label(state: agent_runtime::TurnState) -> &'static str {
    match state {
        agent_runtime::TurnState::Completed => "completed",
        agent_runtime::TurnState::Interrupted => "interrupted",
        _ => "failed",
    }
}
