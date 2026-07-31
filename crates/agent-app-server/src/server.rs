//! The protocol actor (US-016, US-017, US-018).
//!
//! One connection = one read loop and one pump. The read loop decodes, checks
//! ownership and drives the runtime; the pump owns everything that must be
//! ordered against the event stream: the item projection, the pending server
//! requests and the current turn. That split is what makes the correlation of
//! an approval deterministic rather than lucky: the pump polls the runtime
//! events BEFORE its own command queue, so the item a call opens is always
//! projected before the approval that call raises is sent out.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use agent_runtime::thread::{RuntimeEvent, RuntimeEventPayload};
use agent_tools::permission::{ApprovalResponse, PermissionRequest};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::bridge::{ClientBridge, ClientEndpoint};
use crate::host::{HostError, OpenThread, RuntimeHost, ThreadControl};
use crate::items::{Projected, Projector};
use crate::jsonrpc::{ErrorObject, Inbound, Outbound, RequestId, error_code};
use crate::outbound::{MAX_QUEUED_BYTES, MAX_QUEUED_EVENTS, Outbox};
use crate::protocol::{self, *};

/// Default page size of `thread/items/list`, and its ceiling.
pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_PAGE_SIZE: usize = 500;

/// Shared state of every connection of one process.
///
/// This build hosts ONE thread at a time: the tool registry, the Code Mode
/// session and the sub-agent handle of the process each belong to the open
/// thread, so a second live thread would silently rebind them. A second client
/// therefore gets a typed conflict rather than a thread whose tools point
/// elsewhere (edge case #8).
#[derive(Default)]
struct Ownership {
    held: Option<(String, u64)>,
}

pub struct AppServer {
    host: Arc<dyn RuntimeHost>,
    bridge: Arc<ClientBridge>,
    ownership: std::sync::Mutex<Ownership>,
    next_connection: AtomicU64,
}

impl AppServer {
    pub fn new(host: Arc<dyn RuntimeHost>, bridge: Arc<ClientBridge>) -> Arc<Self> {
        Arc::new(Self {
            host,
            bridge,
            ownership: std::sync::Mutex::new(Ownership::default()),
            next_connection: AtomicU64::new(1),
        })
    }

    pub fn connection_id(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    fn ownership(&self) -> std::sync::MutexGuard<'_, Ownership> {
        self.ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claims the write ownership. The refusal names WHICH thread is held, so a
    /// client knows whether to wait or to resume.
    fn claim(&self, thread_id: Option<&str>, connection: u64) -> Result<(), ErrorObject> {
        let mut held = self.ownership();
        match &held.held {
            Some((owned, owner)) if *owner != connection => Err(ErrorObject::new(
                error_code::THREAD_CONFLICT,
                "this server is already driving a thread for another client",
            )
            .with_data(serde_json::json!({ "threadId": owned }))),
            Some((owned, _)) => Err(ErrorObject::new(
                error_code::THREAD_CONFLICT,
                "this connection already holds a thread; unsubscribe from it first",
            )
            .with_data(serde_json::json!({ "threadId": owned }))),
            None => {
                held.held = Some((thread_id.unwrap_or_default().to_string(), connection));
                Ok(())
            }
        }
    }

    /// Names the thread a reservation was taken for, once its identifier is
    /// known. A failed open releases instead.
    fn confirm(&self, thread_id: &str, connection: u64) {
        let mut held = self.ownership();
        if matches!(&held.held, Some((_, owner)) if *owner == connection) {
            held.held = Some((thread_id.to_string(), connection));
        }
    }

    fn release(&self, connection: u64) {
        let mut held = self.ownership();
        if matches!(&held.held, Some((_, owner)) if *owner == connection) {
            held.held = None;
        }
    }
}

/// A live thread, as the read loop sees it.
struct OpenState {
    thread_id: String,
    control: Arc<dyn ThreadControl>,
    commands: mpsc::UnboundedSender<PumpCommand>,
    pump: tokio::task::JoinHandle<()>,
}

/// What the pump must serialize against the event stream.
enum PumpCommand {
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

/// One client connection.
pub struct Connection {
    id: u64,
    server: Arc<AppServer>,
    outbox: Outbox,
    initialized: std::sync::atomic::AtomicBool,
    open: tokio::sync::Mutex<Option<OpenState>>,
}

impl Connection {
    pub fn new(server: Arc<AppServer>, outbox: Outbox) -> Arc<Self> {
        Arc::new(Self {
            id: server.connection_id(),
            server,
            outbox,
            initialized: std::sync::atomic::AtomicBool::new(false),
            open: tokio::sync::Mutex::new(None),
        })
    }

    /// Handles one decoded inbound message. Returns the message to write back,
    /// if any.
    pub async fn handle(self: &Arc<Self>, inbound: Inbound) -> Option<Outbound> {
        match inbound {
            Inbound::Request { id, method, params } => {
                let answer = self.request(&method, params).await;
                Some(match answer {
                    Ok(result) => Outbound::Response { id, result },
                    Err(error) => Outbound::Error {
                        id: Some(id),
                        error,
                    },
                })
            }
            // `initialized` is the only client notification; anything else is
            // ignored on purpose, since a notification has no answer to fail.
            Inbound::Notification { method, .. } => {
                if method != "initialized" {
                    tracing::debug!(
                        target: "pyxis::app_server",
                        method = %method,
                        "ignored client notification"
                    );
                }
                None
            }
            Inbound::Response { id, result } => {
                self.route_response(id, result).await;
                None
            }
        }
    }

    async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, ErrorObject> {
        let request = ClientRequest::decode(method, params)?;
        if !matches!(request, ClientRequest::Initialize(_))
            && !self.initialized.load(Ordering::Acquire)
        {
            return Err(ErrorObject::new(
                error_code::NOT_INITIALIZED,
                format!("`{method}` requires a successful `initialize` first"),
            ));
        }
        match request {
            ClientRequest::Initialize(params) => self.initialize(params),
            ClientRequest::ThreadStart(params) => self.thread_start(params).await,
            ClientRequest::ThreadResume(params) => self.thread_resume(params).await,
            ClientRequest::ThreadUnsubscribe(params) => self.thread_unsubscribe(params).await,
            ClientRequest::ThreadItemsList(params) => self.items_list(params).await,
            ClientRequest::TurnStart(params) => self.turn_start(params).await,
            ClientRequest::TurnSteer(params) => self.turn_steer(params).await,
            ClientRequest::TurnInterrupt(params) => self.turn_interrupt(params).await,
        }
    }

    fn initialize(&self, params: InitializeParams) -> Result<Value, ErrorObject> {
        if self.initialized.load(Ordering::Acquire) {
            return Err(ErrorObject::new(
                error_code::INVALID_REQUEST,
                "this connection is already initialized",
            ));
        }
        let asked = params.protocol_version.unwrap_or(PROTOCOL_VERSION);
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&asked) {
            return Err(ErrorObject::new(
                error_code::UNSUPPORTED_PROTOCOL_VERSION,
                format!("protocol version {asked} is not supported"),
            )
            .with_data(serde_json::json!({ "supported": SUPPORTED_PROTOCOL_VERSIONS })));
        }
        tracing::info!(
            target: "pyxis::app_server",
            client = %params.client_info.name,
            protocol_version = asked,
            "client initialized"
        );
        // Set LAST: nothing mutable happened above, so a refused negotiation
        // leaves the connection exactly as it was (AC1).
        self.initialized.store(true, Ordering::Release);
        json(&InitializeResult {
            server_info: ServerInfo {
                name: self.server.host.server_name(),
                version: self.server.host.server_version(),
            },
            protocol_version: asked,
            capabilities: ServerCapabilities {
                methods: CLIENT_METHODS.iter().map(|m| (*m).to_string()).collect(),
                notifications: SERVER_NOTIFICATIONS
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect(),
                server_requests: SERVER_REQUESTS.iter().map(|m| (*m).to_string()).collect(),
                dynamic_tools: true,
                max_open_threads: 1,
                max_queued_events: MAX_QUEUED_EVENTS as u32,
                max_queued_bytes: MAX_QUEUED_BYTES as u64,
            },
        })
    }

    async fn thread_start(
        self: &Arc<Self>,
        params: ThreadStartParams,
    ) -> Result<Value, ErrorObject> {
        self.server.claim(None, self.id)?;
        let opened = match self.server.host.start_thread(params.dynamic_tools).await {
            Ok(opened) => opened,
            Err(err) => {
                self.server.release(self.id);
                return Err(err.into_error_object());
            }
        };
        self.adopt(opened).await
    }

    async fn thread_resume(
        self: &Arc<Self>,
        params: ThreadResumeParams,
    ) -> Result<Value, ErrorObject> {
        self.server.claim(Some(&params.thread_id), self.id)?;
        let opened = match self
            .server
            .host
            .resume_thread(&params.thread_id, params.dynamic_tools)
            .await
        {
            Ok(opened) => opened,
            Err(err) => {
                self.server.release(self.id);
                return Err(err.into_error_object());
            }
        };
        self.adopt(opened).await
    }

    /// Binds an opened thread to this connection: the pump starts, the tool
    /// pipeline is pointed at this client, and the thread is announced.
    async fn adopt(self: &Arc<Self>, opened: OpenThread) -> Result<Value, ErrorObject> {
        let OpenThread {
            thread_id,
            items,
            events,
            control,
        } = opened;
        self.server.confirm(&thread_id, self.id);
        let item_count = items.len() as u64;
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let pump = Pump {
            thread_id: thread_id.clone(),
            outbox: self.outbox.clone(),
            projector: Projector::resumed(item_count),
            pending: HashMap::new(),
            next_request_id: AtomicI64::new(1),
            current_turn: None,
            queued_inputs: HashMap::new(),
        };
        let handle = tokio::spawn(pump.run(events, commands_rx));
        *self.open.lock().await = Some(OpenState {
            thread_id: thread_id.clone(),
            control,
            commands: commands_tx,
            pump: handle,
        });
        // The tool pipeline points at the connection that OWNS the thread, and
        // only once it does: binding at connect time would let a second client
        // silently take the approvals of the first one's turn.
        self.server
            .bridge
            .bind(Arc::clone(self) as Arc<dyn ClientEndpoint>);
        let _ = self
            .outbox
            .send(notification(&ServerNotification::ThreadStarted(
                ThreadStartedNotification {
                    thread_id: thread_id.clone(),
                },
            )));
        json(&ThreadStartedResult {
            thread_id,
            item_count,
        })
    }

    async fn thread_unsubscribe(self: &Arc<Self>, params: ThreadRef) -> Result<Value, ErrorObject> {
        {
            let open = self.open.lock().await;
            let Some(state) = open.as_ref() else {
                return Err(unknown_thread(&params.thread_id));
            };
            if state.thread_id != params.thread_id {
                return Err(unknown_thread(&params.thread_id));
            }
        }
        self.close_thread("unsubscribed by the client").await;
        json(&EmptyResult {})
    }

    async fn items_list(&self, params: ThreadItemsListParams) -> Result<Value, ErrorObject> {
        let start = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => decode_cursor(cursor, &params.thread_id)?,
        };
        let page_size = params
            .page_size
            .map(|size| (size as usize).clamp(1, MAX_PAGE_SIZE))
            .unwrap_or(DEFAULT_PAGE_SIZE);
        let items = self
            .server
            .host
            .history(&params.thread_id)
            .await
            .map_err(HostError::into_error_object)?;
        let end = start.saturating_add(page_size).min(items.len());
        let page: Vec<ThreadItem> = items.get(start..end).unwrap_or_default().to_vec();
        let next_cursor = (end < items.len()).then(|| encode_cursor(&params.thread_id, end));
        json(&ThreadItemsListResult {
            items: page,
            next_cursor,
        })
    }

    async fn turn_start(&self, params: TurnStartParams) -> Result<Value, ErrorObject> {
        let control = self.control_for(&params.thread_id).await?;
        let turn_id = control
            .submit(InputItem::join(&params.input), params.client_message_id)
            .await
            .map_err(HostError::into_error_object)?;
        json(&TurnStartedResult {
            thread_id: params.thread_id,
            turn_id,
        })
    }

    async fn turn_steer(&self, params: TurnSteerParams) -> Result<Value, ErrorObject> {
        let control = self.control_for(&params.thread_id).await?;
        let turn_id = control
            .steer(
                InputItem::join(&params.input),
                params.client_message_id,
                params.turn_id,
            )
            .await
            .map_err(HostError::into_error_object)?;
        json(&TurnStartedResult {
            thread_id: params.thread_id,
            turn_id,
        })
    }

    async fn turn_interrupt(&self, params: TurnInterruptParams) -> Result<Value, ErrorObject> {
        let control = self.control_for(&params.thread_id).await?;
        control
            .interrupt(params.turn_id)
            .await
            .map_err(HostError::into_error_object)?;
        json(&EmptyResult {})
    }

    async fn control_for(&self, thread_id: &str) -> Result<Arc<dyn ThreadControl>, ErrorObject> {
        let open = self.open.lock().await;
        match open.as_ref() {
            Some(state) if state.thread_id == thread_id => Ok(Arc::clone(&state.control)),
            // A thread this connection does not hold is either owned by someone
            // else or not open at all; both are refusals, never a silent write.
            _ => Err(unknown_thread(thread_id)),
        }
    }

    async fn route_response(&self, id: RequestId, result: Result<Value, ErrorObject>) {
        let open = self.open.lock().await;
        let Some(state) = open.as_ref() else {
            let _ = self.outbox.send(notification(&ServerNotification::Error(
                ErrorNotification {
                    thread_id: None,
                    turn_id: None,
                    message: format!("response {id} arrived with no thread open"),
                },
            )));
            return;
        };
        let _ = state
            .commands
            .send(PumpCommand::ClientResponse { id, result });
    }

    /// Ends the thread this connection holds: the pump stops, the runtime
    /// closes, the ownership is released and the tool pipeline stops pointing
    /// at this client.
    pub async fn close_thread(self: &Arc<Self>, reason: &str) {
        let state = self.open.lock().await.take();
        let Some(state) = state else {
            return;
        };
        self.server.bridge.unbind();
        state.control.close().await;
        // Dropping the command sender ends the pump; the event stream closing
        // would end it too, so whichever happens first wins.
        drop(state.commands);
        let _ = state.pump.await;
        self.server.release(self.id);
        let _ = self
            .outbox
            .send(notification(&ServerNotification::ThreadClosed(
                ThreadClosedNotification {
                    thread_id: state.thread_id,
                    reason: reason.to_string(),
                },
            )));
    }

    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }
}

#[async_trait::async_trait]
impl ClientEndpoint for Connection {
    async fn approve(&self, request: &PermissionRequest) -> Option<ApprovalResponse> {
        let commands = {
            let open = self.open.lock().await;
            open.as_ref().map(|state| state.commands.clone())
        }?;
        let (reply, answer) = oneshot::channel();
        commands
            .send(PumpCommand::Approve {
                request: Box::new(request.clone()),
                reply,
            })
            .ok()?;
        answer.await.ok()
    }

    async fn call_dynamic_tool(
        &self,
        tool: &str,
        call_id: &str,
        arguments: Value,
    ) -> Result<DynamicToolCallResponse, String> {
        let commands = {
            let open = self.open.lock().await;
            open.as_ref().map(|state| state.commands.clone())
        }
        .ok_or_else(|| "no thread is open on this connection".to_string())?;
        let (reply, answer) = oneshot::channel();
        commands
            .send(PumpCommand::DynamicTool {
                tool: tool.to_string(),
                call_id: call_id.to_string(),
                arguments,
                reply,
            })
            .map_err(|_| "the app-server connection is closing".to_string())?;
        answer
            .await
            .map_err(|_| "the app-server connection closed before the tool answered".to_string())?
    }
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

/// Everything ordered against the runtime event stream.
struct Pump {
    thread_id: String,
    outbox: Outbox,
    projector: Projector,
    pending: HashMap<RequestId, PendingRequest>,
    next_request_id: AtomicI64,
    current_turn: Option<String>,
    /// Inputs accepted for a turn that has not started yet.
    queued_inputs: HashMap<String, Vec<String>>,
}

impl Pump {
    async fn run(
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
                        self.emit(ServerNotification::Error(ErrorNotification {
                            thread_id: Some(self.thread_id.clone()),
                            turn_id: self.current_turn.clone(),
                            message: format!(
                                "{dropped} runtime event(s) dropped from the live stream; \
                                 re-read the thread with `thread/items/list`"
                            ),
                        }));
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
                    agent_runtime::TurnState::Running => {
                        // A turn that came back from `needs_input` is the same
                        // turn: it is announced once, not once per resumption.
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
                    state if state.is_terminal() => {
                        let cause_text = cause
                            .clone()
                            .unwrap_or_else(|| format!("turn {}", terminal_label(*state)));
                        // US-019 AC1: read once, by the classifier the TUI, the
                        // headless summary and the durable log all share.
                        let failure =
                            agent_runtime::TurnFailure::classify(*state, cause.as_deref());
                        // Exactly one terminal projection per open item, before
                        // the turn is declared over (US-017 AC3).
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
                                    agent_runtime::TurnState::Interrupted => {
                                        TurnStatus::Interrupted
                                    }
                                    _ => TurnStatus::Failed,
                                },
                                cause: cause.clone(),
                                cause_category: failure
                                    .as_ref()
                                    .map(|failure| failure.category.into()),
                                cause_guidance: failure
                                    .as_ref()
                                    .map(|failure| failure.category.guidance().to_string()),
                            },
                        ));
                        if self.current_turn.as_deref() == Some(turn.as_str()) {
                            self.current_turn = None;
                        }
                    }
                    _ => {}
                }
            }
            RuntimeEventPayload::Engine(engine) => {
                let projected = self.projector.engine(engine);
                self.publish(projected, turn_id);
            }
            RuntimeEventPayload::StoreFailed { operation, detail } => {
                self.emit(ServerNotification::Error(ErrorNotification {
                    thread_id: Some(self.thread_id.clone()),
                    turn_id,
                    message: format!("thread store failed during {operation}: {detail}"),
                }));
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

    fn emit_input(&mut self, turn: &str, text: &str) {
        let item = self.projector.user_message(text.to_string());
        self.emit(ServerNotification::ItemCompleted(ItemNotification {
            thread_id: self.thread_id.clone(),
            turn_id: Some(turn.to_string()),
            item,
        }));
    }

    fn publish(&mut self, projected: Vec<Projected>, turn_id: Option<String>) {
        for one in projected {
            let notification = match one {
                Projected::Started(item) => ServerNotification::ItemStarted(ItemNotification {
                    thread_id: self.thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item,
                }),
                Projected::Completed(item) => ServerNotification::ItemCompleted(ItemNotification {
                    thread_id: self.thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item,
                }),
                Projected::AssistantDelta { item_id, delta } => {
                    ServerNotification::AgentMessageDelta(ItemDeltaNotification {
                        thread_id: self.thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id,
                        delta,
                    })
                }
                Projected::CommandDelta { item_id, delta } => {
                    ServerNotification::CommandExecutionOutputDelta(ItemDeltaNotification {
                        thread_id: self.thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id,
                        delta,
                    })
                }
                Projected::ReasoningDelta { delta } => {
                    ServerNotification::TurnReasoningDelta(TurnDeltaNotification {
                        thread_id: self.thread_id.clone(),
                        turn_id: turn_id.clone(),
                        delta,
                    })
                }
            };
            self.emit(notification);
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
        let item_id = self
            .projector
            .item_for_call(&call_id)
            .unwrap_or_else(|| call_id.clone());
        let correlation = RequestCorrelation {
            thread_id: self.thread_id.clone(),
            turn_id: self.current_turn.clone(),
            item_id: item_id.clone(),
            call_id: call_id.clone(),
        };
        let server_request = match classify(&request.tool) {
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
        let item_id = self
            .projector
            .item_for_call(&call_id)
            .unwrap_or_else(|| call_id.clone());
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

    fn send_request(&mut self, request: ServerRequest, item_id: String, answer: PendingAnswer) {
        let id = RequestId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let method = request.method_name();
        let sent = self.outbox.send(Outbound::Request {
            id: id.clone(),
            method: method.to_string(),
            params: request.params(),
        });
        if sent.is_err() {
            // The queue is closed: nothing will ever answer, so the caller is
            // released now rather than after a timeout it does not have.
            fail_pending(answer, "the client connection is closed");
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
            self.emit(ServerNotification::Error(ErrorNotification {
                thread_id: Some(self.thread_id.clone()),
                turn_id: self.current_turn.clone(),
                message: format!(
                    "response to request {id} refused: it is unknown, already \
                     answered, or belongs to a turn that ended"
                ),
            }));
            return;
        };
        let item_id = pending.item_id.clone();
        let turn_id = pending.turn_id.clone();
        match (pending.answer, result) {
            (PendingAnswer::Approval(reply), Ok(value)) => {
                match serde_json::from_value::<ApprovalDecisionResponse>(value) {
                    Ok(decision) => {
                        let _ = reply.send(match decision.decision {
                            ApprovalDecision::Approved => ApprovalResponse::ALLOW_ONCE,
                            ApprovalDecision::ApprovedForSession => ApprovalResponse::ALLOW_SESSION,
                            ApprovalDecision::Declined => ApprovalResponse::DENY_ONCE,
                            ApprovalDecision::DeclinedForSession => ApprovalResponse::DENY_SESSION,
                        });
                    }
                    // An unreadable decision denies: the fail-closed rule of the
                    // whole permission pipeline does not stop at the wire.
                    Err(err) => {
                        let _ = reply.send(ApprovalResponse::DENY_ONCE);
                        self.emit(ServerNotification::Error(ErrorNotification {
                            thread_id: Some(self.thread_id.clone()),
                            turn_id: turn_id.clone(),
                            message: format!("unreadable approval answer, denied: {err}"),
                        }));
                    }
                }
            }
            (PendingAnswer::Approval(reply), Err(error)) => {
                let _ = reply.send(ApprovalResponse::DENY_ONCE);
                self.emit(ServerNotification::Error(ErrorNotification {
                    thread_id: Some(self.thread_id.clone()),
                    turn_id: turn_id.clone(),
                    message: format!("client refused the approval request: {}", error.message),
                }));
            }
            (PendingAnswer::DynamicTool(reply), Ok(value)) => {
                let _ = reply.send(
                    serde_json::from_value::<DynamicToolCallResponse>(value)
                        .map_err(|err| format!("unreadable tool answer: {err}")),
                );
            }
            (PendingAnswer::DynamicTool(reply), Err(error)) => {
                let _ = reply.send(Err(error.message));
            }
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
    ///
    /// Releasing means DENYING: a request nobody will answer must not leave a
    /// tool waiting, and a permission nobody granted is a permission refused.
    fn cancel_pending(&mut self, turn: Option<&str>) {
        let doomed: Vec<RequestId> = self
            .pending
            .iter()
            .filter(|(_, pending)| match turn {
                None => true,
                Some(turn) => pending.turn_id.as_deref() == Some(turn),
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in doomed {
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            let item_id = pending.item_id.clone();
            let turn_id = pending.turn_id.clone();
            let method = pending.method;
            fail_pending(pending.answer, "the turn ended before the client answered");
            tracing::debug!(
                target: "pyxis::app_server",
                request_id = %id,
                method,
                "server request cancelled"
            );
            self.emit(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    request_id: id.to_string(),
                    thread_id: Some(self.thread_id.clone()),
                    turn_id,
                    item_id: Some(item_id),
                    resolution: ServerRequestResolution::Cancelled,
                },
            ));
        }
    }

    fn emit(&self, notification: ServerNotification) {
        let _ = self.outbox.send(Outbound::Notification {
            method: notification.method_name().to_string(),
            params: notification.params(),
        });
    }
}

fn fail_pending(answer: PendingAnswer, detail: &str) {
    match answer {
        PendingAnswer::Approval(reply) => {
            let _ = reply.send(ApprovalResponse::DENY_ONCE);
        }
        PendingAnswer::DynamicTool(reply) => {
            let _ = reply.send(Err(detail.to_string()));
        }
    }
}

enum ToolFamily {
    Command,
    FileChange,
    Other,
}

fn classify(tool: &str) -> ToolFamily {
    match tool {
        "bash" | "exec_command" | "write_stdin" => ToolFamily::Command,
        "write" | "edit" | "apply_patch" => ToolFamily::FileChange,
        _ => ToolFamily::Other,
    }
}

fn declared_paths(input: &Value) -> Vec<String> {
    ["path", "file_path", "target"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn terminal_label(state: agent_runtime::TurnState) -> &'static str {
    match state {
        agent_runtime::TurnState::Completed => "completed",
        agent_runtime::TurnState::Interrupted => "interrupted",
        _ => "failed",
    }
}

fn notification(notification: &ServerNotification) -> Outbound {
    Outbound::Notification {
        method: notification.method_name().to_string(),
        params: notification.params(),
    }
}

fn unknown_thread(thread_id: &str) -> ErrorObject {
    ErrorObject::new(
        error_code::UNKNOWN_THREAD,
        format!("thread `{thread_id}` is not open on this connection"),
    )
}

fn json<T: serde::Serialize>(value: &T) -> Result<Value, ErrorObject> {
    serde_json::to_value(value)
        .map_err(|err| ErrorObject::new(error_code::INTERNAL_ERROR, err.to_string()))
}

/// Cursors are opaque and bound to their thread: one taken from another thread
/// is refused rather than applied to an unrelated list (FR-17).
fn encode_cursor(thread_id: &str, index: usize) -> String {
    let raw = format!("{thread_id}:{index}");
    raw.bytes().fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

fn decode_cursor(cursor: &str, thread_id: &str) -> Result<usize, ErrorObject> {
    let invalid = || {
        ErrorObject::new(
            error_code::INVALID_PARAMS,
            "cursor is not a cursor this thread handed out",
        )
    };
    if !cursor.len().is_multiple_of(2) {
        return Err(invalid());
    }
    let bytes: Option<Vec<u8>> = cursor
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        })
        .collect();
    let decoded = String::from_utf8(bytes.ok_or_else(invalid)?).map_err(|_| invalid())?;
    let (owner, index) = decoded.rsplit_once(':').ok_or_else(invalid)?;
    if owner != thread_id {
        return Err(invalid());
    }
    index.parse::<usize>().map_err(|_| invalid())
}

/// Re-exported so a transport can name the whole protocol from one module.
pub use protocol::PROTOCOL_VERSION as SERVED_PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_opaque_and_bound_to_its_thread() {
        let cursor = encode_cursor("thr_1", 50);
        assert!(!cursor.contains("thr_1"), "{cursor}");
        assert!(!cursor.contains("50"), "{cursor}");
        assert_eq!(decode_cursor(&cursor, "thr_1"), Ok(50));
        assert!(decode_cursor(&cursor, "thr_2").is_err());
        assert!(decode_cursor("zz", "thr_1").is_err());
        assert!(decode_cursor("abc", "thr_1").is_err());
    }

    #[test]
    fn tool_families_route_to_their_own_approval_request() {
        assert!(matches!(classify("bash"), ToolFamily::Command));
        assert!(matches!(classify("apply_patch"), ToolFamily::FileChange));
        assert!(matches!(classify("mcp__files__read"), ToolFamily::Other));
    }
}
