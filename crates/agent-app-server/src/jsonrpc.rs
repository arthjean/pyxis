//! JSON-RPC 2.0 envelope (US-016).
//!
//! Deliberately hand-rolled rather than pulled from a framework: the app-server
//! needs exactly four message shapes and one property no framework gives for
//! free, which is that a message the server cannot understand still produces a
//! well-formed answer and leaves the connection usable (AC4). Everything above
//! this module works on decoded values; everything below works on bytes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC identifier. Numbers and strings are both legal and are echoed
/// back UNCHANGED: a client that keyed its pending map on `"7"` must not get
/// `7` back.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::Text(s) => f.write_str(s),
        }
    }
}

/// One decoded inbound message.
///
/// A batch is NOT supported and is refused as an invalid request rather than
/// silently taking its first element.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// Answer to a request THIS server sent to the client.
    Response {
        id: RequestId,
        result: Result<Value, ErrorObject>,
    },
}

/// Why a line could not become an [`Inbound`]. Each variant maps to the
/// JSON-RPC error the caller must answer with, and none of them is fatal to
/// the connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    Invalid(String),
}

impl DecodeError {
    pub fn into_error_object(self) -> ErrorObject {
        match self {
            Self::Parse(detail) => ErrorObject::new(error_code::PARSE_ERROR, detail),
            Self::Invalid(detail) => ErrorObject::new(error_code::INVALID_REQUEST, detail),
        }
    }
}

/// Decodes one line. `id` is extracted even from a malformed request so the
/// refusal can be correlated; a message with no usable id becomes a
/// null-id error response, which is what JSON-RPC prescribes.
pub fn decode(line: &str) -> Result<Inbound, (Option<RequestId>, DecodeError)> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| (None, DecodeError::Parse(err.to_string())))?;
    if value.is_array() {
        return Err((
            None,
            DecodeError::Invalid("batched requests are not supported".into()),
        ));
    }
    let Some(object) = value.as_object() else {
        return Err((
            None,
            DecodeError::Invalid("a message must be a JSON object".into()),
        ));
    };
    let id = object.get("id").and_then(|raw| match raw {
        Value::Number(n) => n.as_i64().map(RequestId::Number),
        Value::String(s) => Some(RequestId::Text(s.clone())),
        _ => None,
    });
    match object.get("jsonrpc").and_then(Value::as_str) {
        Some(JSONRPC_VERSION) => {}
        Some(other) => {
            return Err((
                id,
                DecodeError::Invalid(format!("unsupported jsonrpc version `{other}`")),
            ));
        }
        None => {
            return Err((
                id,
                DecodeError::Invalid("missing `jsonrpc` version field".into()),
            ));
        }
    }

    match object.get("method").map(|method| method.as_str()) {
        Some(Some(method)) => {
            let params = object.get("params").cloned().unwrap_or(Value::Null);
            let method = method.to_string();
            Ok(match id {
                Some(id) => Inbound::Request { id, method, params },
                None => Inbound::Notification { method, params },
            })
        }
        Some(None) => Err((id, DecodeError::Invalid("`method` must be a string".into()))),
        // No method: it is an answer to one of OUR requests.
        None => {
            let Some(id) = id else {
                return Err((
                    None,
                    DecodeError::Invalid("a response must carry an `id`".into()),
                ));
            };
            if let Some(error) = object.get("error") {
                let error: ErrorObject = serde_json::from_value(error.clone()).map_err(|err| {
                    (
                        Some(id.clone()),
                        DecodeError::Invalid(format!("malformed error object: {err}")),
                    )
                })?;
                return Ok(Inbound::Response {
                    id,
                    result: Err(error),
                });
            }
            match object.get("result") {
                Some(result) => Ok(Inbound::Response {
                    id,
                    result: Ok(result.clone()),
                }),
                None => Err((
                    Some(id),
                    DecodeError::Invalid("a response needs `result` or `error`".into()),
                )),
            }
        }
    }
}

/// JSON-RPC error codes. The reserved range is used as specified; everything
/// Pyxis adds sits in the implementation-defined server range.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// A method was called before `initialize` succeeded.
    pub const NOT_INITIALIZED: i32 = -32000;
    /// The client asked for a protocol version this server does not speak.
    pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32001;
    /// Another connection owns the thread (edge case #8).
    pub const THREAD_CONFLICT: i32 = -32002;
    /// The named thread is not open on this connection.
    pub const UNKNOWN_THREAD: i32 = -32003;
    /// The named turn is not the turn that is running.
    pub const UNKNOWN_TURN: i32 = -32004;
    /// The runtime refused the operation (mailbox full, shutting down, store).
    pub const RUNTIME_REFUSED: i32 = -32005;
    /// The client is too far behind and its queue is closed (US-018 AC4).
    pub const BACKPRESSURE: i32 = -32006;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One outbound message, ready to be serialized.
#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        /// `None` when the inbound message carried no usable id.
        id: Option<RequestId>,
        error: ErrorObject,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

impl Outbound {
    /// Serializes to one line. Field order is fixed here rather than derived so
    /// a golden fixture of the wire stays byte-stable.
    pub fn to_line(&self) -> String {
        let value = match self {
            Self::Response { id, result } => serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "result": result,
            }),
            Self::Error { id, error } => serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "error": error,
            }),
            Self::Request { id, method, params } => serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "method": method,
                "params": params,
            }),
            Self::Notification { method, params } => serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "method": method,
                "params": params,
            }),
        };
        value.to_string()
    }

    /// Byte cost of the message, as the backpressure accounting sees it
    /// (US-018 AC4).
    pub fn weight(&self) -> usize {
        self.to_line().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_a_notification_and_a_response_are_told_apart() {
        let request = decode(r#"{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{}}"#);
        assert!(matches!(
            request,
            Ok(Inbound::Request { ref method, .. }) if method == "turn/start"
        ));
        let notification = decode(r#"{"jsonrpc":"2.0","method":"initialized"}"#);
        assert!(matches!(
            notification,
            Ok(Inbound::Notification { ref method, .. }) if method == "initialized"
        ));
        let response = decode(r#"{"jsonrpc":"2.0","id":"a","result":{"decision":"approved"}}"#);
        assert!(matches!(
            response,
            Ok(Inbound::Response {
                id: RequestId::Text(ref id),
                result: Ok(_),
            }) if id == "a"
        ));
    }

    /// AC4: a broken message names its fault and keeps its identifier when it
    /// has one, so the client can fail the right pending call.
    #[test]
    fn malformed_messages_map_to_their_json_rpc_code() {
        let (id, error) = decode("{not json").expect_err("invalid JSON");
        assert_eq!(id, None);
        assert_eq!(error.into_error_object().code, error_code::PARSE_ERROR);

        let (id, error) = decode(r#"{"id":4,"method":"x"}"#).expect_err("no version");
        assert_eq!(id, Some(RequestId::Number(4)));
        assert_eq!(error.into_error_object().code, error_code::INVALID_REQUEST);

        let (_, error) =
            decode(r#"[{"jsonrpc":"2.0","id":1,"method":"x"}]"#).expect_err("a batch is refused");
        assert_eq!(error.into_error_object().code, error_code::INVALID_REQUEST);

        let (id, error) =
            decode(r#"{"jsonrpc":"2.0","id":9}"#).expect_err("neither result nor error");
        assert_eq!(id, Some(RequestId::Number(9)));
        assert_eq!(error.into_error_object().code, error_code::INVALID_REQUEST);
    }

    #[test]
    fn an_identifier_round_trips_with_its_json_type() {
        let line = Outbound::Response {
            id: RequestId::Text("7".into()),
            result: serde_json::json!({}),
        }
        .to_line();
        assert!(line.contains(r#""id":"7""#), "{line}");
        let line = Outbound::Response {
            id: RequestId::Number(7),
            result: serde_json::json!({}),
        }
        .to_line();
        assert!(line.contains(r#""id":7"#), "{line}");
    }

    #[test]
    fn an_error_without_an_identifier_still_answers() {
        let line = Outbound::Error {
            id: None,
            error: ErrorObject::new(error_code::PARSE_ERROR, "boom"),
        }
        .to_line();
        assert!(line.contains(r#""id":null"#), "{line}");
        assert!(line.contains(r#""code":-32700"#), "{line}");
    }
}
