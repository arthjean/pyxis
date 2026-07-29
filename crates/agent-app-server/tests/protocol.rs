//! Acceptance of the app-server contract (EP-005).
//!
//! Everything is driven against a scripted [`FakeHost`]: no provider, no
//! credential, no sandbox. What is under test is the protocol, the item
//! projection, the correlation of server requests and the ownership rules, and
//! all four are properties of this crate alone.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_app_server::bridge::{BridgeApprover, ClientBridge, dynamic_tools};
use agent_app_server::host::{HostError, OpenThread, RuntimeHost, ThreadControl};
use agent_app_server::items::project_messages;
use agent_app_server::outbound::{Outbox, OutboxReceiver};
use agent_app_server::protocol::{DynamicToolSpec, ThreadItem};
use agent_app_server::server::{AppServer, Connection};
use agent_app_server::transport::handle_line;
use agent_core::event::{AgentEvent, ToolCallView, ToolResultView};
use agent_core::message::{Message, ToolCallId};
use agent_core::tools::ToolResultStatus;
use agent_runtime::id::{EventId, SequentialIds, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::thread::{RuntimeEvent, RuntimeEventPayload};
use agent_tools::permission::{Approver, PermissionRequest};
use serde_json::{Value, json};
use tokio::sync::broadcast;

const THREAD_ID: &str = "thr_00000000000000000000000000000001";
const TURN_ID: &str = "trn_00000000000000000000000000000002";

// ───────────────────────── scripted host ─────────────────────────

struct FakeControl {
    submitted: std::sync::Mutex<Vec<String>>,
    interrupted: std::sync::Mutex<Vec<Option<String>>>,
    closed: AtomicU64,
}

#[async_trait::async_trait]
impl ThreadControl for FakeControl {
    async fn submit(
        &self,
        text: String,
        _client_message_id: Option<String>,
    ) -> Result<String, HostError> {
        self.submitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text);
        Ok(TURN_ID.to_string())
    }

    async fn steer(
        &self,
        text: String,
        _client_message_id: Option<String>,
        expected_turn: Option<String>,
    ) -> Result<String, HostError> {
        if matches!(expected_turn.as_deref(), Some(turn) if turn != TURN_ID) {
            return Err(HostError::StaleTurn);
        }
        self.submitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text);
        Ok(TURN_ID.to_string())
    }

    async fn interrupt(&self, turn_id: Option<String>) -> Result<(), HostError> {
        self.interrupted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(turn_id);
        Ok(())
    }

    async fn close(&self) {
        self.closed.fetch_add(1, Ordering::Relaxed);
    }
}

struct FakeHost {
    events: broadcast::Sender<RuntimeEvent>,
    control: Arc<FakeControl>,
    history: std::sync::Mutex<Vec<ThreadItem>>,
    bridge: Arc<ClientBridge>,
    /// Tools the client declared, as the registry would hold them.
    registered: std::sync::Mutex<Vec<String>>,
}

impl FakeHost {
    fn new(bridge: Arc<ClientBridge>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            events,
            control: Arc::new(FakeControl {
                submitted: std::sync::Mutex::new(Vec::new()),
                interrupted: std::sync::Mutex::new(Vec::new()),
                closed: AtomicU64::new(0),
            }),
            history: std::sync::Mutex::new(Vec::new()),
            bridge,
            registered: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn open(&self, dynamic: Vec<DynamicToolSpec>) -> OpenThread {
        // What the binary does with the declared tools: build them and register
        // them. Names are recorded so a test can assert they entered a registry.
        let (tools, _rejected) = dynamic_tools(&self.bridge, &dynamic);
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(tools.iter().map(|tool| tool.name().to_string()));
        OpenThread {
            thread_id: THREAD_ID.to_string(),
            items: self
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            events: self.events.subscribe(),
            control: Arc::clone(&self.control) as Arc<dyn ThreadControl>,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeHost for FakeHost {
    async fn start_thread(&self, dynamic: Vec<DynamicToolSpec>) -> Result<OpenThread, HostError> {
        Ok(self.open(dynamic))
    }

    async fn resume_thread(
        &self,
        thread_id: &str,
        dynamic: Vec<DynamicToolSpec>,
    ) -> Result<OpenThread, HostError> {
        if thread_id != THREAD_ID {
            return Err(HostError::UnknownThread(thread_id.to_string()));
        }
        Ok(self.open(dynamic))
    }

    async fn history(&self, thread_id: &str) -> Result<Vec<ThreadItem>, HostError> {
        if thread_id != THREAD_ID {
            return Err(HostError::UnknownThread(thread_id.to_string()));
        }
        Ok(self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

// ───────────────────────── harness ─────────────────────────

struct Harness {
    connection: Arc<Connection>,
    receiver: OutboxReceiver,
    host: Arc<FakeHost>,
    ids: SequentialIds,
}

impl Harness {
    fn new() -> Self {
        let bridge = ClientBridge::new();
        let host = FakeHost::new(Arc::clone(&bridge));
        Self::with_host(host, bridge)
    }

    fn with_host(host: Arc<FakeHost>, bridge: Arc<ClientBridge>) -> Self {
        let server = AppServer::new(Arc::clone(&host) as Arc<dyn RuntimeHost>, bridge);
        Self::on_server(server, host)
    }

    fn on_server(server: Arc<AppServer>, host: Arc<FakeHost>) -> Self {
        let (outbox, receiver) = Outbox::new();
        let connection = Connection::new(server, outbox);
        Self {
            connection,
            receiver,
            host,
            ids: SequentialIds::new(),
        }
    }

    async fn send(&self, line: &str) {
        handle_line(&self.connection, line)
            .await
            .expect("the connection survives every client message");
    }

    /// Next outbound message, or a failure: a test that waits forever tells
    /// nothing about what broke.
    async fn next(&self) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(5), self.receiver.recv())
            .await
            .expect("the server answered in time")
            .expect("the queue is open");
        serde_json::from_str(&message.to_line()).expect("valid JSON")
    }

    /// Next message whose `method` is `method`, skipping the rest.
    async fn next_notification(&self, method: &str) -> Value {
        loop {
            let message = self.next().await;
            if message.get("method").and_then(Value::as_str) == Some(method) {
                return message;
            }
        }
    }

    async fn call(&self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string())
            .await;
        loop {
            let message = self.next().await;
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    async fn initialize(&self) -> Value {
        self.call(
            1,
            "initialize",
            json!({"clientInfo": {"name": "test-client"}}),
        )
        .await
    }

    async fn open_thread(&self) {
        self.initialize().await;
        let answer = self.call(2, "thread/start", json!({})).await;
        assert_eq!(answer["result"]["threadId"], THREAD_ID);
    }

    /// Publishes a runtime event and lets the pump drain it.
    fn emit(&self, turn: Option<&str>, payload: RuntimeEventPayload) {
        let event = RuntimeEvent {
            event_id: EventId::generate(&self.ids),
            thread_id: THREAD_ID.parse().expect("thread id"),
            turn_id: turn.map(|id| id.parse::<TurnId>().expect("turn id")),
            payload,
        };
        self.host.events.send(event).expect("a subscriber is live");
    }

    fn engine(&self, event: AgentEvent) {
        self.emit(Some(TURN_ID), RuntimeEventPayload::Engine(event));
    }

    fn turn_running(&self) {
        self.emit(
            Some(TURN_ID),
            RuntimeEventPayload::TurnStateChanged {
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
            },
        );
    }

    fn turn_terminal(&self, to: TurnState, cause: Option<&str>) {
        self.emit(
            Some(TURN_ID),
            RuntimeEventPayload::TurnStateChanged {
                from: Some(TurnState::Running),
                to,
                cause: cause.map(str::to_string),
            },
        );
    }
}

fn permission_request(tool: &str, call_id: &str) -> PermissionRequest {
    PermissionRequest {
        call_id: ToolCallId::from(call_id.to_string()),
        tool: tool.to_string(),
        reason: "a confirmation is required".into(),
        taint_forced: false,
        mode: "Default".into(),
        input_summary: "rm -rf build".into(),
        input: json!({"command": "rm -rf build"}),
        memoizable: true,
        memo_refused: None,
    }
}

// ───────────────────────── US-016 ─────────────────────────

/// AC1: capabilities are published by `initialize`, and nothing is served
/// before it.
#[tokio::test]
async fn nothing_is_served_before_initialize() {
    let harness = Harness::new();
    let refused = harness.call(1, "thread/start", json!({})).await;
    assert_eq!(refused["error"]["code"], -32000);
    assert!(harness.host.control.submitted().is_empty());

    let ready = harness.initialize().await;
    let capabilities = &ready["result"]["capabilities"];
    assert_eq!(ready["result"]["protocolVersion"], 1);
    assert!(
        capabilities["methods"]
            .as_array()
            .expect("methods")
            .contains(&json!("turn/start"))
    );
    assert!(
        capabilities["serverRequests"]
            .as_array()
            .expect("server requests")
            .contains(&json!("item/commandExecution/requestApproval"))
    );
    assert_eq!(capabilities["maxQueuedEvents"], 1024);
    assert_eq!(capabilities["dynamicTools"], true);
}

/// AC4: three broken messages, three JSON-RPC answers, and a connection that
/// still serves the next request.
#[tokio::test]
async fn a_broken_message_answers_and_the_connection_survives() {
    let harness = Harness::new();
    harness.initialize().await;

    harness.send("{ not json").await;
    assert_eq!(harness.next().await["error"]["code"], -32700);

    let unknown = harness.call(7, "thread/teleport", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);

    // Re-initializing an initialized connection is itself a refusal.
    let again = harness
        .call(8, "initialize", json!({"clientInfo": {"name": "c"}}))
        .await;
    assert_eq!(again["error"]["code"], -32600);

    // Still alive, and still initialized.
    let opened = harness.call(9, "thread/start", json!({})).await;
    assert_eq!(opened["result"]["threadId"], THREAD_ID);

    // A version this build does not speak is refused with the list it does,
    // and the connection stays usable for a second attempt.
    let fresh = Harness::new();
    let version = fresh
        .call(
            1,
            "initialize",
            json!({"clientInfo": {"name": "c"}, "protocolVersion": 99}),
        )
        .await;
    assert_eq!(version["error"]["code"], -32001);
    assert_eq!(version["error"]["data"]["supported"], json!([1]));
    assert_eq!(fresh.initialize().await["result"]["protocolVersion"], 1);
}

/// AC2: a turn produces `turn/started`, ordered items with stable identifiers,
/// and `turn/completed`.
#[tokio::test]
async fn a_turn_streams_ordered_items_under_stable_identifiers() {
    let harness = Harness::new();
    harness.open_thread().await;

    let started = harness
        .call(
            3,
            "turn/start",
            json!({"threadId": THREAD_ID, "input": [{"type": "text", "text": "hello"}]}),
        )
        .await;
    assert_eq!(started["result"]["turnId"], TURN_ID);
    assert_eq!(harness.host.control.submitted(), vec!["hello".to_string()]);

    harness.emit(
        Some(TURN_ID),
        RuntimeEventPayload::InputAccepted {
            text: "hello".into(),
        },
    );
    harness.turn_running();
    harness.engine(AgentEvent::Text("Hi".into()));
    harness.engine(AgentEvent::Text(" there".into()));
    harness.engine(AgentEvent::EndTurn);
    harness.turn_terminal(TurnState::Completed, None);

    let turn_started = harness.next_notification("turn/started").await;
    assert_eq!(turn_started["params"]["turnId"], TURN_ID);

    let user = harness.next_notification("item/completed").await;
    assert_eq!(user["params"]["item"]["type"], "userMessage");
    assert_eq!(user["params"]["item"]["id"], "item_0");

    let assistant_started = harness.next_notification("item/started").await;
    assert_eq!(assistant_started["params"]["item"]["id"], "item_1");

    let delta = harness.next_notification("item/agentMessage/delta").await;
    assert_eq!(delta["params"]["itemId"], "item_1");
    assert_eq!(delta["params"]["delta"], "Hi");

    let assistant = harness.next_notification("item/completed").await;
    assert_eq!(assistant["params"]["item"]["id"], "item_1");
    assert_eq!(assistant["params"]["item"]["text"], "Hi there");

    let completed = harness.next_notification("turn/completed").await;
    assert_eq!(completed["params"]["status"], "completed");
}

/// AC2 again, on the failure side: a terminal cause reaches the client instead
/// of the turn simply stopping (FR-19).
#[tokio::test]
async fn a_failed_turn_carries_its_cause() {
    let harness = Harness::new();
    harness.open_thread().await;
    harness.turn_running();
    harness.turn_terminal(TurnState::Failed, Some("provider unreachable"));

    let completed = harness.next_notification("turn/completed").await;
    assert_eq!(completed["params"]["status"], "failed");
    assert_eq!(completed["params"]["cause"], "provider unreachable");
}

/// AC3: two clients, one writer. The loser is told which thread is held and
/// nothing of its request took effect.
#[tokio::test]
async fn a_second_client_gets_a_typed_conflict() {
    let bridge = ClientBridge::new();
    let host = FakeHost::new(Arc::clone(&bridge));
    let server = AppServer::new(Arc::clone(&host) as Arc<dyn RuntimeHost>, bridge);
    let first = Harness::on_server(Arc::clone(&server), Arc::clone(&host));
    let second = Harness::on_server(server, Arc::clone(&host));

    first.open_thread().await;
    second.initialize().await;
    let refused = second
        .call(2, "thread/resume", json!({"threadId": THREAD_ID}))
        .await;
    assert_eq!(refused["error"]["code"], -32002);
    assert_eq!(refused["error"]["data"]["threadId"], THREAD_ID);

    // And it cannot write to the thread it does not own either.
    let write = second
        .call(
            3,
            "turn/start",
            json!({"threadId": THREAD_ID, "input": [{"type": "text", "text": "mine"}]}),
        )
        .await;
    assert_eq!(write["error"]["code"], -32003);
    assert_eq!(first.host.control.submitted(), Vec::<String>::new());

    // Released, the thread is claimable again.
    first
        .call(4, "thread/unsubscribe", json!({"threadId": THREAD_ID}))
        .await;
    let taken = second
        .call(5, "thread/resume", json!({"threadId": THREAD_ID}))
        .await;
    assert_eq!(taken["result"]["threadId"], THREAD_ID);
}

/// Opening a thread announces it, so a client that shares its connection with
/// another surface learns about the thread without polling.
#[tokio::test]
async fn opening_a_thread_announces_it() {
    let harness = Harness::new();
    harness.initialize().await;
    harness
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}).to_string())
        .await;
    let announced = harness.next_notification("thread/started").await;
    assert_eq!(announced["params"]["threadId"], THREAD_ID);
}

/// The tool pipeline points at the connection that OWNS the thread, and at no
/// other: a second client connecting mid-turn must not receive the approvals
/// of the first one.
#[tokio::test]
async fn approvals_reach_the_owner_and_not_a_late_connection() {
    let bridge = ClientBridge::new();
    let host = FakeHost::new(Arc::clone(&bridge));
    let server = AppServer::new(Arc::clone(&host) as Arc<dyn RuntimeHost>, bridge);
    let owner = Harness::on_server(Arc::clone(&server), Arc::clone(&host));
    owner.open_thread().await;
    owner.turn_running();

    // A second client connects and initializes but owns nothing.
    let latecomer = Harness::on_server(server, Arc::clone(&host));
    latecomer.initialize().await;

    let approver_bridge = Arc::clone(&owner.host.bridge);
    let asked = tokio::spawn(async move {
        BridgeApprover::new(approver_bridge)
            .approve(&permission_request("bash", "call_1"))
            .await
    });
    let request = owner
        .next_notification("item/commandExecution/requestApproval")
        .await;
    let request_id = request["id"].as_i64().expect("a request id");
    owner
        .send(
            &json!({"jsonrpc":"2.0","id":request_id,"result":{"decision":"approved"}}).to_string(),
        )
        .await;
    assert!(asked.await.expect("answered").allow);

    // Nothing was ever written to the latecomer beyond its own answers.
    let pending = tokio::time::timeout(Duration::from_millis(50), latecomer.receiver.recv()).await;
    assert!(pending.is_err(), "the latecomer received {pending:?}");
}

// ───────────────────────── US-017 ─────────────────────────

/// AC1: an approval carries the four identifiers and resolves exactly once.
/// AC4: the second answer is refused without re-running anything.
#[tokio::test]
async fn an_approval_correlates_and_resolves_once() {
    let harness = Harness::new();
    harness.open_thread().await;
    harness.turn_running();
    harness.engine(AgentEvent::ToolCall(ToolCallView {
        id: ToolCallId::from("call_1".to_string()),
        name: "bash".into(),
        input: json!({"command": "rm -rf build"}),
    }));

    let bridge = Arc::clone(&harness.host.bridge);
    let asked = tokio::spawn(async move {
        BridgeApprover::new(bridge)
            .approve(&permission_request("bash", "call_1"))
            .await
    });

    let request = harness
        .next_notification("item/commandExecution/requestApproval")
        .await;
    let params = &request["params"];
    assert_eq!(params["threadId"], THREAD_ID);
    assert_eq!(params["turnId"], TURN_ID);
    // The item the call opened, projected BEFORE the approval went out.
    assert_eq!(params["itemId"], "item_0");
    assert_eq!(params["callId"], "call_1");
    assert_eq!(params["memoizable"], true);
    let request_id = request["id"].as_i64().expect("a request id");

    harness
        .send(
            &json!({"jsonrpc":"2.0","id":request_id,"result":{"decision":"approvedForSession"}})
                .to_string(),
        )
        .await;
    let answer = asked.await.expect("the approver answered");
    assert!(answer.allow && answer.remember);

    let resolved = harness.next_notification("serverRequest/resolved").await;
    assert_eq!(resolved["params"]["resolution"], "answered");
    assert_eq!(resolved["params"]["itemId"], "item_0");

    // AC4: answering twice changes nothing and is reported.
    harness
        .send(
            &json!({"jsonrpc":"2.0","id":request_id,"result":{"decision":"declined"}}).to_string(),
        )
        .await;
    let refusal = harness.next_notification("error").await;
    assert!(
        refusal["params"]["message"]
            .as_str()
            .expect("a message")
            .contains("refused"),
        "{refusal}"
    );
}

/// AC1 fail-closed: an answer the server cannot read denies, it never guesses.
#[tokio::test]
async fn an_unreadable_approval_answer_denies() {
    let harness = Harness::new();
    harness.open_thread().await;
    harness.turn_running();

    let bridge = Arc::clone(&harness.host.bridge);
    let asked = tokio::spawn(async move {
        BridgeApprover::new(bridge)
            .approve(&permission_request("write", "call_9"))
            .await
    });

    let request = harness
        .next_notification("item/fileChange/requestApproval")
        .await;
    let request_id = request["id"].as_i64().expect("a request id");
    harness
        .send(&json!({"jsonrpc":"2.0","id":request_id,"result":{"decision":"maybe"}}).to_string())
        .await;
    let answer = asked.await.expect("the approver answered");
    assert!(!answer.allow, "an unreadable decision must deny");
    assert!(!answer.remember);
}

/// AC3: an interruption closes every open item exactly once and cancels the
/// approval nobody will answer.
#[tokio::test]
async fn an_interruption_closes_items_and_cancels_pending_requests() {
    let harness = Harness::new();
    harness.open_thread().await;
    harness.turn_running();
    harness.engine(AgentEvent::ToolCall(ToolCallView {
        id: ToolCallId::from("call_1".to_string()),
        name: "bash".into(),
        input: json!({"command": "sleep 100"}),
    }));

    let bridge = Arc::clone(&harness.host.bridge);
    let asked = tokio::spawn(async move {
        BridgeApprover::new(bridge)
            .approve(&permission_request("bash", "call_1"))
            .await
    });
    harness
        .next_notification("item/commandExecution/requestApproval")
        .await;

    let interrupted = harness
        .call(
            5,
            "turn/interrupt",
            json!({"threadId": THREAD_ID, "turnId": TURN_ID}),
        )
        .await;
    assert!(interrupted.get("error").is_none(), "{interrupted}");
    assert_eq!(
        harness.host.control.interrupted(),
        vec![Some(TURN_ID.to_string())]
    );
    harness.turn_terminal(TurnState::Interrupted, Some("interrupted by the client"));

    // The pending approval is released as a DENIAL: a permission nobody
    // granted is a permission refused.
    let answer = asked.await.expect("the approver was released");
    assert!(!answer.allow);

    let closed = harness.next_notification("item/completed").await;
    assert_eq!(closed["params"]["item"]["id"], "item_0");
    assert_eq!(closed["params"]["item"]["status"], "failed");

    let resolved = harness.next_notification("serverRequest/resolved").await;
    assert_eq!(resolved["params"]["resolution"], "cancelled");

    let completed = harness.next_notification("turn/completed").await;
    assert_eq!(completed["params"]["status"], "interrupted");
}

/// AC2: a client-declared tool is an ordinary registry tool, and its execution
/// travels back over the same correlation as an approval.
#[tokio::test]
async fn a_dynamic_tool_travels_the_ordinary_dispatch() {
    let harness = Harness::new();
    harness.initialize().await;
    let opened = harness
        .call(
            2,
            "thread/start",
            json!({"dynamicTools": [{
                "name": "lookup",
                "description": "Looks a record up in the client",
                "inputSchema": {
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "additionalProperties": false
                }
            }]}),
        )
        .await;
    assert_eq!(opened["result"]["threadId"], THREAD_ID);
    assert_eq!(harness.host.registered(), vec!["lookup".to_string()]);

    harness.turn_running();
    harness.engine(AgentEvent::ToolCall(ToolCallView {
        id: ToolCallId::from("call_7".to_string()),
        name: "lookup".into(),
        input: json!({"id": "42"}),
    }));

    // The tool the registry would run, invoked exactly as the registry does.
    let (tools, rejected) = dynamic_tools(
        &harness.host.bridge,
        &[DynamicToolSpec {
            name: "lookup".into(),
            description: "d".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            }),
        }],
    );
    assert!(rejected.is_empty());
    let tool = tools.into_iter().next().expect("one tool");
    let ctx = agent_tools::tool::ToolCtx::new(std::env::temp_dir())
        .for_call(ToolCallId::from("call_7".to_string()), Arc::new(|_| {}));
    let called = tokio::spawn(async move { tool.invoke(json!({"id": "42"}), &ctx).await });

    let request = harness.next_notification("item/tool/call").await;
    assert_eq!(request["params"]["tool"], "lookup");
    assert_eq!(request["params"]["callId"], "call_7");
    assert_eq!(request["params"]["itemId"], "item_0");
    assert_eq!(request["params"]["arguments"]["id"], "42");
    let request_id = request["id"].as_i64().expect("a request id");

    harness
        .send(
            &json!({"jsonrpc":"2.0","id":request_id,"result":{"content":"record 42","isError":false}})
                .to_string(),
        )
        .await;
    let output = called
        .await
        .expect("joined")
        .expect("the client answered the call");
    assert_eq!(output.content, "record 42");
    assert!(!output.is_error);
}

/// AC2, refusal side: a tool the client declares badly is not registered, and
/// the model never sees it.
#[tokio::test]
async fn a_malformed_dynamic_tool_is_not_registered() {
    let harness = Harness::new();
    harness.initialize().await;
    harness
        .call(
            2,
            "thread/start",
            json!({"dynamicTools": [{
                "name": "not a name",
                "description": "d",
                "inputSchema": {"type": "object"}
            }]}),
        )
        .await;
    assert!(harness.host.registered().is_empty());
}

// ───────────────────────── US-018 ─────────────────────────

/// AC1: every item appears exactly once, in persistent order, across pages.
#[tokio::test]
async fn the_history_pages_without_gap_or_repeat() {
    let harness = Harness::new();
    let mut messages = Vec::new();
    for index in 0..60 {
        messages.push(Message::user(format!("ask {index}")));
        messages.push(Message::assistant_text(format!("answer {index}")));
    }
    let items = project_messages(&messages);
    assert_eq!(items.len(), 120);
    *harness
        .host
        .history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = items.clone();

    harness.open_thread().await;
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut request_id = 10;
    loop {
        let mut params = json!({"threadId": THREAD_ID, "pageSize": 25});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page = harness.call(request_id, "thread/items/list", params).await;
        request_id += 1;
        for item in page["result"]["items"].as_array().expect("items") {
            seen.push(
                item["id"]
                    .as_str()
                    .expect("every item is identified")
                    .to_string(),
            );
        }
        match page["result"]["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
        assert!(request_id < 20, "pagination must terminate");
    }
    let expected: Vec<String> = items.iter().map(|item| item.id().to_string()).collect();
    assert_eq!(seen, expected);
}

/// A cursor is opaque and belongs to its thread: one from elsewhere is refused
/// rather than applied to an unrelated list.
#[tokio::test]
async fn a_foreign_cursor_is_refused() {
    let harness = Harness::new();
    harness.open_thread().await;
    let refused = harness
        .call(
            11,
            "thread/items/list",
            json!({"threadId": THREAD_ID, "cursor": "deadbeef"}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602);
}

/// A resumed thread numbers its live items after the history it reopened, so
/// the two surfaces of one thread never hand out the same identifier twice.
#[tokio::test]
async fn a_resumed_thread_continues_its_numbering() {
    let harness = Harness::new();
    *harness
        .host
        .history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = project_messages(&[
        Message::user("earlier"),
        Message::assistant_text("earlier answer"),
    ]);
    harness.initialize().await;
    let resumed = harness
        .call(2, "thread/resume", json!({"threadId": THREAD_ID}))
        .await;
    assert_eq!(resumed["result"]["itemCount"], 2);

    harness.turn_running();
    harness.engine(AgentEvent::ToolCall(ToolCallView {
        id: ToolCallId::from("call_1".to_string()),
        name: "read".into(),
        input: json!({"path": "a.rs"}),
    }));
    let started = harness.next_notification("item/started").await;
    assert_eq!(started["params"]["item"]["id"], "item_2");

    harness.engine(AgentEvent::ToolResult(ToolResultView {
        id: ToolCallId::from("call_1".to_string()),
        content: "fn main() {}".into(),
        status: Some(ToolResultStatus::Success),
        structured_content: None,
        is_error: false,
        error_kind: None,
        untrusted: true,
        duration_ms: None,
        truncation: None,
        execution: None,
    }));
    let completed = harness.next_notification("item/completed").await;
    assert_eq!(completed["params"]["item"]["id"], "item_2");
    assert_eq!(completed["params"]["item"]["status"], "completed");
    assert_eq!(completed["params"]["item"]["untrusted"], true);
}

/// A thread event that belongs to no turn (an agent relation) must not be
/// projected as a turn artifact.
#[tokio::test]
async fn a_thread_scoped_event_projects_nothing() {
    let harness = Harness::new();
    harness.open_thread().await;
    harness.emit(None, RuntimeEventPayload::ShuttingDown);
    let closed = harness.next_notification("thread/closed").await;
    assert_eq!(closed["params"]["threadId"], THREAD_ID);
}

impl FakeControl {
    fn submitted(&self) -> Vec<String> {
        self.submitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn interrupted(&self) -> Vec<Option<String>> {
        self.interrupted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl FakeHost {
    fn registered(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
