//! The protocol actor (US-016, US-017, US-018).
//!
//! One connection = one read loop and one pump. This module owns the read loop:
//! it decodes, checks ownership and drives the runtime. Everything that must be
//! ordered against the event stream lives in [`crate::pump`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_tools::permission::{ApprovalResponse, PermissionRequest};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{ClientBridge, ClientEndpoint};
use crate::cursor;
use crate::host::{HostError, OpenThread, RuntimeHost, ThreadControl};
use crate::items::Projector;
use crate::jsonrpc::{ErrorObject, Inbound, Outbound, RequestId, error_code};
use crate::outbound::{MAX_QUEUED_BYTES, MAX_QUEUED_EVENTS, Outbox};
use crate::protocol::*;
use crate::pump::{Pump, PumpCommand};

/// Default page size of `thread/items/list`, and its ceiling.
pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_PAGE_SIZE: usize = 500;

/// Who holds the single thread this process hosts.
///
/// This build hosts ONE thread at a time: the tool registry, the Code Mode
/// session and the sub-agent handle of the process each belong to the open
/// thread, so a second live thread would silently rebind them. A second client
/// therefore gets a typed conflict rather than a thread whose tools point
/// elsewhere (edge case #8).
enum Ownership {
    Free,
    /// A connection is opening a thread whose identifier is not known yet.
    Reserved {
        owner: u64,
    },
    Bound {
        thread_id: String,
        owner: u64,
    },
}

impl Ownership {
    fn owner(&self) -> Option<u64> {
        match self {
            Self::Free => None,
            Self::Reserved { owner } | Self::Bound { owner, .. } => Some(*owner),
        }
    }

    /// What to tell a client that cannot have the thread, and `None` when it
    /// can. The refusal names WHICH thread is held, so a client knows whether
    /// to wait or to resume; `threadId` is null while the holder is still
    /// opening one, because at that point it has no name to give.
    fn conflict(&self, connection: u64) -> Option<ErrorObject> {
        let (message, thread_id) = match self {
            Self::Free => return None,
            Self::Reserved { owner } if *owner == connection => {
                ("this connection is already opening a thread", Value::Null)
            }
            Self::Reserved { .. } => (
                "this server is already opening a thread for another client",
                Value::Null,
            ),
            Self::Bound { thread_id, owner } if *owner == connection => (
                "this connection already holds a thread; unsubscribe from it first",
                Value::String(thread_id.clone()),
            ),
            Self::Bound { thread_id, .. } => (
                "this server is already driving a thread for another client",
                Value::String(thread_id.clone()),
            ),
        };
        Some(
            ErrorObject::new(error_code::THREAD_CONFLICT, message)
                .with_data(serde_json::json!({ "threadId": thread_id })),
        )
    }
}

pub struct AppServer {
    host: Arc<dyn RuntimeHost>,
    ownership: std::sync::Mutex<Ownership>,
    bridge: Arc<ClientBridge>,
    next_connection: AtomicU64,
}

impl AppServer {
    pub fn new(host: Arc<dyn RuntimeHost>, bridge: Arc<ClientBridge>) -> Arc<Self> {
        Arc::new(Self {
            host,
            bridge,
            ownership: std::sync::Mutex::new(Ownership::Free),
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

    /// Reserves the write ownership for `connection`.
    ///
    /// The reservation releases itself unless it is confirmed, so no failure
    /// path between here and a live thread can leak the slot.
    fn reserve(self: &Arc<Self>, connection: u64) -> Result<Reservation, ErrorObject> {
        let mut held = self.ownership();
        if let Some(conflict) = held.conflict(connection) {
            return Err(conflict);
        }
        *held = Ownership::Reserved { owner: connection };
        drop(held);
        Ok(Reservation {
            server: Arc::clone(self),
            connection,
            confirmed: false,
        })
    }

    fn release(&self, connection: u64) {
        let mut held = self.ownership();
        if held.owner() == Some(connection) {
            *held = Ownership::Free;
        }
    }
}

/// A claim on the single thread slot, released on drop until it is confirmed.
struct Reservation {
    server: Arc<AppServer>,
    connection: u64,
    confirmed: bool,
}

impl Reservation {
    /// Names the thread the reservation was taken for, once it exists.
    fn confirm(mut self, thread_id: &str) {
        let mut held = self.server.ownership();
        if held.owner() == Some(self.connection) {
            *held = Ownership::Bound {
                thread_id: thread_id.to_string(),
                owner: self.connection,
            };
        }
        self.confirmed = true;
        // Released before `self` drops, which would take the same lock.
        drop(held);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.confirmed {
            self.server.release(self.connection);
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

/// One client connection.
pub struct Connection {
    id: u64,
    server: Arc<AppServer>,
    outbox: Outbox,
    initialized: AtomicBool,
    open: tokio::sync::Mutex<Option<OpenState>>,
}

impl Connection {
    pub fn new(server: Arc<AppServer>, outbox: Outbox) -> Arc<Self> {
        Arc::new(Self {
            id: server.connection_id(),
            server,
            outbox,
            initialized: AtomicBool::new(false),
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
        let reservation = self.server.reserve(self.id)?;
        let opened = self
            .server
            .host
            .start_thread(params.dynamic_tools)
            .await
            .map_err(HostError::into_error_object)?;
        self.adopt(opened, reservation).await
    }

    async fn thread_resume(
        self: &Arc<Self>,
        params: ThreadResumeParams,
    ) -> Result<Value, ErrorObject> {
        let reservation = self.server.reserve(self.id)?;
        let opened = self
            .server
            .host
            .resume_thread(&params.thread_id, params.dynamic_tools)
            .await
            .map_err(HostError::into_error_object)?;
        self.adopt(opened, reservation).await
    }

    /// Binds an opened thread to this connection: the pump starts, the tool
    /// pipeline is pointed at this client, and the thread is announced.
    async fn adopt(
        self: &Arc<Self>,
        opened: OpenThread,
        reservation: Reservation,
    ) -> Result<Value, ErrorObject> {
        let OpenThread {
            thread_id,
            items,
            events,
            control,
        } = opened;
        reservation.confirm(&thread_id);
        let item_count = items.len() as u64;
        let pump = Pump::new(
            thread_id.clone(),
            self.outbox.clone(),
            Projector::resumed(item_count),
        );
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
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
        self.notify(ServerNotification::ThreadStarted(
            ThreadStartedNotification {
                thread_id: thread_id.clone(),
            },
        ));
        json(&ThreadStartedResult {
            thread_id,
            item_count,
        })
    }

    async fn thread_unsubscribe(self: &Arc<Self>, params: ThreadRef) -> Result<Value, ErrorObject> {
        {
            let open = self.open.lock().await;
            let held = open.as_ref().map(|state| state.thread_id.as_str());
            if held != Some(params.thread_id.as_str()) {
                return Err(unknown_thread(&params.thread_id));
            }
        }
        self.close_thread("unsubscribed by the client").await;
        json(&EmptyResult {})
    }

    async fn items_list(&self, params: ThreadItemsListParams) -> Result<Value, ErrorObject> {
        let start = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => cursor::decode(cursor, &params.thread_id)?,
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
        let next_cursor = (end < items.len()).then(|| cursor::encode(&params.thread_id, end));
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

    /// The pump queue of the open thread, when there is one.
    async fn commands(&self) -> Option<mpsc::UnboundedSender<PumpCommand>> {
        let open = self.open.lock().await;
        open.as_ref().map(|state| state.commands.clone())
    }

    async fn route_response(&self, id: RequestId, result: Result<Value, ErrorObject>) {
        let Some(commands) = self.commands().await else {
            self.notify(ServerNotification::Error(ErrorNotification {
                thread_id: None,
                turn_id: None,
                message: format!("response {id} arrived with no thread open"),
            }));
            return;
        };
        let _ = commands.send(PumpCommand::ClientResponse { id, result });
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
        self.notify(ServerNotification::ThreadClosed(ThreadClosedNotification {
            thread_id: state.thread_id,
            reason: reason.to_string(),
        }));
    }

    fn notify(&self, notification: ServerNotification) {
        let _ = self.outbox.send(notification.into());
    }

    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }
}

#[async_trait::async_trait]
impl ClientEndpoint for Connection {
    async fn approve(&self, request: &PermissionRequest) -> Option<ApprovalResponse> {
        let commands = self.commands().await?;
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
        let commands = self
            .commands()
            .await
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
