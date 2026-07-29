//! The wire surface of the app-server (US-016, US-017, US-018).
//!
//! Three declarative tables (client requests, server notifications, server
//! requests) generate at once: the decoded enum the dispatcher matches on, the
//! list of exposed names, and the schema entries. Coverage of the published
//! schema is therefore a property of the code rather than of a checklist: a
//! method that is not in the table cannot be dispatched, and one that is in it
//! cannot be missing from the schema (US-018 AC3).
//!
//! Wire names are the Codex baseline names. Where Pyxis exposes something Codex
//! has no equivalent for, the divergence is marked in the doc comment of the
//! entry and repeated in `docs/app-server/README.md`.

use schemars::{JsonSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonrpc::{ErrorObject, error_code};

/// Protocol version this build speaks. Bumped when an already published shape
/// changes; adding a method or an optional field does not change how an
/// existing client reads the stream and therefore does not bump it.
pub const PROTOCOL_VERSION: u32 = 1;
/// Every version this build accepts in `initialize`.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// One entry of the published schema.
pub struct MethodSchema {
    pub method: &'static str,
    pub params: Value,
    /// `None` for a notification: it has no answer.
    pub result: Option<Value>,
}

macro_rules! client_requests {
    ($( $(#[doc = $doc:expr])* $variant:ident => $wire:literal { params: $params:ty, result: $result:ty } ),* $(,)?) => {
        /// A decoded client request.
        #[derive(Debug, Clone, PartialEq)]
        pub enum ClientRequest {
            $( $(#[doc = $doc])* $variant($params), )*
        }

        /// Every method this server answers, in table order.
        pub const CLIENT_METHODS: &[&str] = &[ $($wire),* ];

        impl ClientRequest {
            /// Decodes `params` for `method`.
            ///
            /// An unknown method and an unreadable payload are two DIFFERENT
            /// JSON-RPC codes, because a client fixes them differently.
            pub fn decode(method: &str, params: Value) -> Result<Self, ErrorObject> {
                match method {
                    $(
                        $wire => {
                            let params = decode_params::<$params>(params, $wire)?;
                            Ok(Self::$variant(params))
                        }
                    )*
                    other => Err(ErrorObject::new(
                        error_code::METHOD_NOT_FOUND,
                        format!("unknown method `{other}`"),
                    )
                    .with_data(serde_json::json!({ "methods": CLIENT_METHODS }))),
                }
            }

            pub const fn method_name(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => $wire, )*
                }
            }
        }

        fn client_request_schemas(generator: &mut SchemaGenerator) -> Vec<MethodSchema> {
            vec![
                $(
                    MethodSchema {
                        method: $wire,
                        params: generator.subschema_for::<$params>().to_value(),
                        result: Some(generator.subschema_for::<$result>().to_value()),
                    },
                )*
            ]
        }
    };
}

macro_rules! server_notifications {
    ($( $(#[doc = $doc:expr])* $variant:ident => $wire:literal ($payload:ty) ),* $(,)?) => {
        /// A notification the server pushes to the client.
        #[derive(Debug, Clone, PartialEq)]
        pub enum ServerNotification {
            $( $(#[doc = $doc])* $variant($payload), )*
        }

        pub const SERVER_NOTIFICATIONS: &[&str] = &[ $($wire),* ];

        impl ServerNotification {
            pub const fn method_name(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => $wire, )*
                }
            }

            /// Serializes the payload. A payload that cannot serialize would be
            /// a bug in this crate, never client input, so it degrades to an
            /// empty object rather than taking the connection down.
            pub fn params(&self) -> Value {
                match self {
                    $(
                        Self::$variant(payload) => serde_json::to_value(payload)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                    )*
                }
            }
        }

        fn server_notification_schemas(generator: &mut SchemaGenerator) -> Vec<MethodSchema> {
            vec![
                $(
                    MethodSchema {
                        method: $wire,
                        params: generator.subschema_for::<$payload>().to_value(),
                        result: None,
                    },
                )*
            ]
        }
    };
}

macro_rules! server_requests {
    ($( $(#[doc = $doc:expr])* $variant:ident => $wire:literal { params: $params:ty, response: $response:ty } ),* $(,)?) => {
        /// A request the SERVER sends to the client and waits an answer for.
        #[derive(Debug, Clone, PartialEq)]
        pub enum ServerRequest {
            $( $(#[doc = $doc])* $variant($params), )*
        }

        pub const SERVER_REQUESTS: &[&str] = &[ $($wire),* ];

        impl ServerRequest {
            pub const fn method_name(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => $wire, )*
                }
            }

            pub fn params(&self) -> Value {
                match self {
                    $(
                        Self::$variant(payload) => serde_json::to_value(payload)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                    )*
                }
            }
        }

        fn server_request_schemas(generator: &mut SchemaGenerator) -> Vec<MethodSchema> {
            vec![
                $(
                    MethodSchema {
                        method: $wire,
                        params: generator.subschema_for::<$params>().to_value(),
                        result: Some(generator.subschema_for::<$response>().to_value()),
                    },
                )*
            ]
        }
    };
}

fn decode_params<T: for<'de> Deserialize<'de>>(
    params: Value,
    method: &str,
) -> Result<T, ErrorObject> {
    // An absent `params` is legal for a method whose parameters are all
    // optional: it reads as the empty object rather than as a null.
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params).map_err(|err| {
        ErrorObject::new(
            error_code::INVALID_PARAMS,
            format!("invalid params for `{method}`: {err}"),
        )
    })
}

client_requests! {
    /// Negotiates the protocol version and publishes the server capabilities.
    /// Nothing else is served until it succeeds (AC1).
    Initialize => "initialize" { params: InitializeParams, result: InitializeResult },
    /// Opens a brand-new thread and takes its write ownership.
    ThreadStart => "thread/start" { params: ThreadStartParams, result: ThreadStartedResult },
    /// Reopens a durable thread and takes its write ownership.
    ThreadResume => "thread/resume" { params: ThreadResumeParams, result: ThreadStartedResult },
    /// Releases the ownership and closes the thread on this server.
    ThreadUnsubscribe => "thread/unsubscribe" { params: ThreadRef, result: EmptyResult },
    /// Reads the persisted items of a thread, one page at a time.
    ThreadItemsList => "thread/items/list" { params: ThreadItemsListParams, result: ThreadItemsListResult },
    /// Opens a turn on a thread.
    TurnStart => "turn/start" { params: TurnStartParams, result: TurnStartedResult },
    /// Feeds an input into the turn that is running.
    TurnSteer => "turn/steer" { params: TurnSteerParams, result: TurnStartedResult },
    /// Interrupts the turn that is running.
    TurnInterrupt => "turn/interrupt" { params: TurnInterruptParams, result: EmptyResult },
}

server_notifications! {
    /// A failure that belongs to no single request.
    Error => "error" (ErrorNotification),
    ThreadStarted => "thread/started" (ThreadStartedNotification),
    /// The server released the thread: shutdown, or the owning connection left.
    ThreadClosed => "thread/closed" (ThreadClosedNotification),
    TurnStarted => "turn/started" (TurnStartedNotification),
    TurnCompleted => "turn/completed" (TurnCompletedNotification),
    ItemStarted => "item/started" (ItemNotification),
    ItemCompleted => "item/completed" (ItemNotification),
    AgentMessageDelta => "item/agentMessage/delta" (ItemDeltaNotification),
    CommandExecutionOutputDelta => "item/commandExecution/outputDelta" (ItemDeltaNotification),
    /// PYXIS DIVERGENCE. Codex attaches reasoning to an item; Pyxis never
    /// persists reasoning, so binding it to an item would make the item numbering
    /// of a resumed thread disagree with its log. It is turn-scoped instead.
    TurnReasoningDelta => "turn/reasoning/delta" (TurnDeltaNotification),
    /// A pending server request stopped waiting for the client.
    ServerRequestResolved => "serverRequest/resolved" (ServerRequestResolvedNotification),
}

server_requests! {
    /// Approval of a command execution (`bash`, `exec_command`, `write_stdin`).
    CommandExecutionRequestApproval => "item/commandExecution/requestApproval" {
        params: CommandExecutionApprovalParams,
        response: ApprovalDecisionResponse
    },
    /// Approval of a file change (`write`, `edit`, `apply_patch`).
    FileChangeRequestApproval => "item/fileChange/requestApproval" {
        params: FileChangeApprovalParams,
        response: ApprovalDecisionResponse
    },
    /// PYXIS DIVERGENCE. Codex has one approval request per item family; Pyxis
    /// dispatches generic tools (MCP, multi-agent, Code Mode) through the same
    /// pipeline, so they need an approval request of their own rather than being
    /// forced into a command or a file change they are not.
    ToolRequestApproval => "item/tool/requestApproval" {
        params: ToolApprovalParams,
        response: ApprovalDecisionResponse
    },
    /// Executes a client-side dynamic tool the model called.
    DynamicToolCall => "item/tool/call" {
        params: DynamicToolCallParams,
        response: DynamicToolCallResponse
    },
}

// ───────────────────────── shared identifiers ─────────────────────────

/// Names a thread. Used on its own as the parameter of `thread/unsubscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRef {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmptyResult {}

// ───────────────────────── initialize ─────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    /// Protocol version the client asks for. Absent means "the oldest this
    /// build still speaks", never "whatever the server prefers".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub server_info: ServerInfo,
    pub protocol_version: u32,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// What this build can actually do. Published BEFORE any mutation so a client
/// never discovers a missing capability by having a call fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Methods served by this build.
    pub methods: Vec<String>,
    /// Notifications this build may push.
    pub notifications: Vec<String>,
    /// Requests this build may send to the client. A client that answers none
    /// of them will see every approval fail closed.
    pub server_requests: Vec<String>,
    /// Client-declared tools may be registered at `thread/start`.
    pub dynamic_tools: bool,
    /// How many threads this server hosts at once. One, in this build: the tool
    /// registry, the Code Mode session and the sub-agent handle of the process
    /// each belong to the open thread.
    pub max_open_threads: u32,
    /// Queue bound past which a slow client is closed (US-018 AC4).
    pub max_queued_events: u32,
    pub max_queued_bytes: u64,
}

// ───────────────────────── threads ─────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadStartParams {
    /// Tools the CLIENT executes. They enter the same registry, the same
    /// permission pipeline and the same dispatch as the built-in ones
    /// (US-017 AC2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_tools: Vec<DynamicToolSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadResumeParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_tools: Vec<DynamicToolSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedResult {
    pub thread_id: String,
    /// Items already in the durable log. Read them with `thread/items/list`.
    pub item_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadItemsListParams {
    pub thread_id: String,
    /// Opaque cursor returned by the previous page. Absent starts at the first
    /// item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemsListResult {
    pub items: Vec<ThreadItem>,
    /// Absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ───────────────────────── turns ─────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<InputItem>,
    /// Idempotency key. Two `turn/start` carrying the same key open one turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnSteerParams {
    pub thread_id: String,
    /// The turn the client believes it is correcting. Naming a turn that is no
    /// longer running is refused instead of becoming a new turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub input: Vec<InputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnInterruptParams {
    pub thread_id: String,
    /// Absent interrupts whatever is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedResult {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputItem {
    Text { text: String },
}

impl InputItem {
    /// Flattens an input list into the single text a submission carries.
    pub fn join(items: &[Self]) -> String {
        items
            .iter()
            .map(|item| match item {
                Self::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ───────────────────────── items ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
}

/// One projected element of a thread. The same shapes are served by
/// `thread/items/list` and by `item/started` / `item/completed`, so a client
/// writes one renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    #[serde(rename_all = "camelCase")]
    UserMessage { id: String, text: String },
    #[serde(rename_all = "camelCase")]
    AssistantMessage { id: String, text: String },
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        id: String,
        call_id: String,
        command: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        status: ItemStatus,
    },
    #[serde(rename_all = "camelCase")]
    FileChange {
        id: String,
        call_id: String,
        /// Workspace-relative paths the call declared it would touch.
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
        status: ItemStatus,
    },
    /// Everything the two families above do not cover: MCP, multi-agent, Code
    /// Mode, reads, and every client-side dynamic tool.
    #[serde(rename_all = "camelCase")]
    ToolCall {
        id: String,
        call_id: String,
        tool: String,
        input: Value,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
        status: ItemStatus,
        /// The result carries untrusted content (taint).
        untrusted: bool,
    },
    #[serde(rename_all = "camelCase")]
    Error { id: String, message: String },
}

impl ThreadItem {
    pub fn id(&self) -> &str {
        match self {
            Self::UserMessage { id, .. }
            | Self::AssistantMessage { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileChange { id, .. }
            | Self::ToolCall { id, .. }
            | Self::Error { id, .. } => id,
        }
    }

    pub fn status(&self) -> ItemStatus {
        match self {
            Self::UserMessage { .. } | Self::AssistantMessage { .. } => ItemStatus::Completed,
            Self::Error { .. } => ItemStatus::Failed,
            Self::CommandExecution { status, .. }
            | Self::FileChange { status, .. }
            | Self::ToolCall { status, .. } => *status,
        }
    }
}

// ───────────────────────── notifications ─────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedNotification {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadClosedNotification {
    pub thread_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub status: TurnStatus,
    /// Terminal cause, as the durable log recorded it. Present whenever the
    /// runtime recorded one, which is what makes a failure diagnosable instead
    /// of silent (FR-19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub item: ThreadItem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemDeltaNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnDeltaNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub delta: String,
}

/// Why a pending server request stopped waiting (US-017 AC3/AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ServerRequestResolution {
    /// The client answered it.
    Answered,
    /// The turn was interrupted or the thread closed under it. The client must
    /// drop the dialog: an answer sent afterwards is refused.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
    /// The `id` of the server request, as a string whatever its JSON type.
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub resolution: ServerRequestResolution,
}

// ───────────────────────── server requests ─────────────────────────

// Correlation every server request carries: four identifiers, which is what
// makes exactly-once resolution possible on the client side too (US-017 AC1).
// Flattened into each request, so a client reads `threadId`, `turnId`,
// `itemId` and `callId` next to the payload rather than under a nested object.
// Deliberately undocumented as a type: `#[serde(flatten)]` is inlined by the
// schema derivation, and a doc comment here would end up describing whichever
// request inlined it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestCorrelation {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub item_id: String,
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionApprovalParams {
    #[serde(flatten)]
    pub correlation: RequestCorrelation,
    pub command: String,
    pub reason: String,
    /// The request was forced by recent untrusted content: approving it opts
    /// out of the prompt-injection defense for this call.
    pub taint_forced: bool,
    /// The permission mode in force when the request was raised.
    pub mode: String,
    /// The answer may be remembered for the session.
    pub memoizable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo_refused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    #[serde(flatten)]
    pub correlation: RequestCorrelation,
    pub tool: String,
    pub paths: Vec<String>,
    /// The tool input, verbatim, so a client can render a real diff.
    pub input: Value,
    pub reason: String,
    pub taint_forced: bool,
    pub mode: String,
    pub memoizable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo_refused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolApprovalParams {
    #[serde(flatten)]
    pub correlation: RequestCorrelation,
    pub tool: String,
    pub input: Value,
    pub summary: String,
    pub reason: String,
    pub taint_forced: bool,
    pub mode: String,
    pub memoizable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo_refused: Option<String>,
}

/// The four answers a client may give: allow or refuse, once or for the
/// session. Aborting the turn is NOT one of them, because that is
/// `turn/interrupt` and it must reach the model and the tools, not just this
/// one call. A `*ForSession` answer is remembered only when the request said it
/// was memoizable; the pipeline, never the client, decides that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Approved,
    ApprovedForSession,
    Declined,
    DeclinedForSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecisionResponse {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    #[serde(flatten)]
    pub correlation: RequestCorrelation,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DynamicToolCallResponse {
    /// What the model reads. Untrusted like any other tool output.
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

/// A tool the client executes on the model's behalf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The whole published surface, as one document.
pub fn protocol_schemas() -> (
    Vec<MethodSchema>,
    Vec<MethodSchema>,
    Vec<MethodSchema>,
    Value,
) {
    let mut generator = SchemaGenerator::default();
    let requests = client_request_schemas(&mut generator);
    let notifications = server_notification_schemas(&mut generator);
    let server = server_request_schemas(&mut generator);
    let definitions = Value::Object(generator.take_definitions(false));
    (requests, notifications, server, definitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_method_is_told_apart_from_bad_parameters() {
        let unknown = ClientRequest::decode("thread/teleport", Value::Null).expect_err("unknown");
        assert_eq!(unknown.code, error_code::METHOD_NOT_FOUND);
        // The refusal carries what IS served, so a client discovers the surface
        // without reading the source.
        assert!(unknown.data.is_some());

        let bad = ClientRequest::decode("turn/start", serde_json::json!({"threadId": 12}))
            .expect_err("bad params");
        assert_eq!(bad.code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn absent_params_read_as_an_empty_object_only_where_that_is_legal() {
        assert!(matches!(
            ClientRequest::decode("thread/start", Value::Null),
            Ok(ClientRequest::ThreadStart(_))
        ));
        // `turn/start` needs a thread and an input: absent params is a refusal,
        // never a turn opened on a guessed thread.
        assert!(ClientRequest::decode("turn/start", Value::Null).is_err());
    }

    /// An unexpected field is a client bug, and a silently ignored one is a
    /// client bug that ships. Parameters deny what they do not know.
    #[test]
    fn an_unknown_parameter_field_is_refused() {
        let err = ClientRequest::decode(
            "turn/interrupt",
            serde_json::json!({"threadId": "t", "turnID": "typo"}),
        )
        .expect_err("typo");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn the_three_tables_are_disjoint_and_non_empty() {
        assert!(!CLIENT_METHODS.is_empty());
        assert!(!SERVER_NOTIFICATIONS.is_empty());
        assert!(!SERVER_REQUESTS.is_empty());
        for method in CLIENT_METHODS {
            assert!(!SERVER_NOTIFICATIONS.contains(method), "{method}");
            assert!(!SERVER_REQUESTS.contains(method), "{method}");
        }
    }

    #[test]
    fn items_serialize_with_their_discriminant() {
        let item = ThreadItem::CommandExecution {
            id: "item_3".into(),
            call_id: "call_1".into(),
            command: "ls".into(),
            output: String::new(),
            exit_code: None,
            status: ItemStatus::InProgress,
        };
        let json = serde_json::to_value(&item).expect("serializable");
        assert_eq!(json["type"], "commandExecution");
        assert_eq!(json["status"], "inProgress");
        assert_eq!(json["callId"], "call_1");
        // An absent output stays out of the wire rather than travelling as "".
        assert!(json.get("output").is_none());
        assert_eq!(
            serde_json::from_value::<ThreadItem>(json).expect("round-trip"),
            item
        );
    }
}
