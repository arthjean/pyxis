//! What the tool pipeline asks of whichever client is driving (US-017).
//!
//! Two surfaces cross this boundary in the same direction, tool -> client:
//! an approval, and the execution of a tool the CLIENT owns. Both are built
//! into the process before any thread exists (the registry is assembled once),
//! so they cannot hold a connection: they hold a slot a connection binds itself
//! into, and every one of them is FAIL-CLOSED when that slot is empty. An
//! app-server with no client attached refuses instead of approving.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_tools::error::{ToolError, ValidationError};
use agent_tools::permission::{
    ApprovalMemo, ApprovalResponse, Approver, PermCtx, PermissionDecision, PermissionRequest,
};
use agent_tools::tool::{DynTool, MAX_TOOL_INPUT_BYTES, ToolCtx, ToolOutput, estimate_json_bytes};
use serde_json::Value;

use crate::protocol::{DynamicToolCallResponse, DynamicToolSpec};

/// Timeout of a client-side tool call. Long enough for a human-in-the-loop
/// client, bounded so a silent client cannot pin a turn forever.
pub const DYNAMIC_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound of a declared dynamic tool schema, same reasoning as MCP: a client
/// does not get to flood the prompt through its schemas.
pub const MAX_DYNAMIC_SCHEMA_BYTES: usize = 16_384;

/// The connection side of the bridge.
#[async_trait::async_trait]
pub trait ClientEndpoint: Send + Sync {
    /// Asks the client to decide. `None` means the client is gone or did not
    /// answer: the caller then denies, it never assumes.
    async fn approve(&self, request: &PermissionRequest) -> Option<ApprovalResponse>;

    /// Runs a tool the client declared. `Err` carries what the model reads.
    async fn call_dynamic_tool(
        &self,
        tool: &str,
        call_id: &str,
        arguments: Value,
    ) -> Result<DynamicToolCallResponse, String>;
}

/// Process-level slot. Built before the registry, bound by whichever connection
/// owns the open thread.
#[derive(Default)]
pub struct ClientBridge {
    endpoint: Mutex<Option<Arc<dyn ClientEndpoint>>>,
}

impl ClientBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn bind(&self, endpoint: Arc<dyn ClientEndpoint>) {
        *self.lock() = Some(endpoint);
    }

    /// Releases the slot. Every later approval fails closed until another
    /// connection binds.
    pub fn unbind(&self) {
        *self.lock() = None;
    }

    fn endpoint(&self) -> Option<Arc<dyn ClientEndpoint>> {
        self.lock().clone()
    }

    /// A poisoned lock is recovered rather than propagated: the tool pipeline
    /// must not panic on a permission check.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<dyn ClientEndpoint>>> {
        self.endpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// [`Approver`] over the bridge. Denies once, and remembers nothing, whenever
/// no client can answer.
pub struct BridgeApprover {
    bridge: Arc<ClientBridge>,
}

impl BridgeApprover {
    pub fn new(bridge: Arc<ClientBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl Approver for BridgeApprover {
    async fn approve(&self, request: &PermissionRequest) -> ApprovalResponse {
        let Some(endpoint) = self.bridge.endpoint() else {
            return ApprovalResponse::DENY_ONCE;
        };
        endpoint
            .approve(request)
            .await
            .unwrap_or(ApprovalResponse::DENY_ONCE)
    }
}

/// A tool the client executes. Registered into the SAME registry as the
/// built-in ones, so it meets the same permission mode, the same taint defense,
/// the same hooks and the same cancellation (US-017 AC2).
pub struct ClientTool {
    name: String,
    description: String,
    input_schema: Value,
    bridge: Arc<ClientBridge>,
}

#[async_trait::async_trait]
impl DynTool for ClientTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    // Fail-closed metadata, on the MCP reasoning: what a remote party declares
    // about itself is a hint, never a security decision on our side.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_sensitive(&self) -> bool {
        true
    }

    fn is_taint_sensitive(&self) -> bool {
        true
    }

    fn returns_untrusted(&self) -> bool {
        true
    }

    fn behavioral_guidelines(&self) -> &[&'static str] {
        &[]
    }

    fn precheck(&self, raw: &Value, _ctx: &ToolCtx) -> Result<(), ToolError> {
        let estimated = estimate_json_bytes(raw);
        if estimated > MAX_TOOL_INPUT_BYTES {
            return Err(ToolError::Validation(ValidationError::new(format!(
                "tool input too large: estimated {estimated} bytes > {MAX_TOOL_INPUT_BYTES}"
            ))));
        }
        match raw {
            Value::Object(_) | Value::Null => Ok(()),
            _ => Err(ToolError::Parse(
                "dynamic tool arguments must be a JSON object".to_string(),
            )),
        }
    }

    /// A confirmation by default. The client declared the tool; it did not get
    /// to declare that running it is safe.
    fn permission(&self, _raw: &Value, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    /// Never remembered: the arguments are free-form, so nothing identifies
    /// "the same act" twice.
    fn approval_memo(&self, _raw: &Value) -> ApprovalMemo {
        ApprovalMemo::NotApplicable
    }

    fn timeout(&self, _ctx: &ToolCtx) -> Duration {
        DYNAMIC_TOOL_TIMEOUT
    }

    async fn invoke(&self, raw: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let Some(endpoint) = self.bridge.endpoint() else {
            return Err(ToolError::Io(format!(
                "dynamic tool `{}`: no app-server client is attached",
                self.name
            )));
        };
        // The identifier the registry dispatched under: the same one the item
        // and the approval carry, so the client correlates the three.
        let call_id = ctx
            .call_id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .unwrap_or_default();
        match endpoint.call_dynamic_tool(&self.name, &call_id, raw).await {
            Ok(response) if response.is_error => {
                let mut output = ToolOutput::error(response.content);
                if let Some(structured) = response.structured_content {
                    output = output.with_structured_content(structured);
                }
                Ok(output)
            }
            Ok(response) => {
                let mut output = ToolOutput::text(response.content);
                if let Some(structured) = response.structured_content {
                    output = output.with_structured_content(structured);
                }
                Ok(output)
            }
            Err(detail) => Err(ToolError::Io(format!(
                "dynamic tool `{}`: {detail}",
                self.name
            ))),
        }
    }
}

/// Why a declared dynamic tool was not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedTool {
    pub name: String,
    pub reason: String,
}

/// Turns client-declared specs into registrable tools.
///
/// A spec that cannot be exposed is REJECTED and named, never silently
/// dropped: a client whose tool never gets called must be able to know why.
pub fn dynamic_tools(
    bridge: &Arc<ClientBridge>,
    specs: &[DynamicToolSpec],
) -> (Vec<Box<dyn DynTool>>, Vec<RejectedTool>) {
    let mut tools: Vec<Box<dyn DynTool>> = Vec::new();
    let mut rejected = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    for spec in specs {
        let reject = |reason: &str| RejectedTool {
            name: spec.name.clone(),
            reason: reason.to_string(),
        };
        if !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || spec.name.is_empty()
            || spec.name.len() > 64
        {
            rejected.push(reject("name must be 1 to 64 characters of [A-Za-z0-9_-]"));
            continue;
        }
        if taken.contains(&spec.name) {
            rejected.push(reject("name already declared by this client"));
            continue;
        }
        let bytes = estimate_json_bytes(&spec.input_schema);
        if bytes > MAX_DYNAMIC_SCHEMA_BYTES {
            rejected.push(reject(&format!(
                "input schema too large ({bytes} bytes > {MAX_DYNAMIC_SCHEMA_BYTES})"
            )));
            continue;
        }
        let spec_for_provider = agent_core::provider::ToolSpec::function(
            spec.name.clone(),
            spec.description.clone(),
            spec.input_schema.clone(),
        );
        if let Err(err) = spec_for_provider.validate() {
            rejected.push(reject(&err.to_string()));
            continue;
        }
        taken.push(spec.name.clone());
        tools.push(Box::new(ClientTool {
            name: spec.name.clone(),
            description: spec.description.clone(),
            input_schema: spec.input_schema.clone(),
            bridge: Arc::clone(bridge),
        }));
    }
    (tools, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::message::ToolCallId;

    fn request() -> PermissionRequest {
        PermissionRequest {
            call_id: ToolCallId::from("c1".to_string()),
            tool: "bash".into(),
            reason: "test".into(),
            taint_forced: false,
            mode: "Default".into(),
            input_summary: "ls".into(),
            input: serde_json::json!({"command": "ls"}),
            memoizable: false,
            memo_refused: None,
        }
    }

    struct Silent;

    #[async_trait::async_trait]
    impl ClientEndpoint for Silent {
        async fn approve(&self, _request: &PermissionRequest) -> Option<ApprovalResponse> {
            None
        }

        async fn call_dynamic_tool(
            &self,
            _tool: &str,
            _call_id: &str,
            _arguments: Value,
        ) -> Result<DynamicToolCallResponse, String> {
            Err("gone".into())
        }
    }

    #[tokio::test]
    async fn an_unbound_bridge_denies() {
        let bridge = ClientBridge::new();
        let approver = BridgeApprover::new(Arc::clone(&bridge));
        assert_eq!(
            approver.approve(&request()).await,
            ApprovalResponse::DENY_ONCE
        );
    }

    #[tokio::test]
    async fn a_client_that_does_not_answer_denies_and_remembers_nothing() {
        let bridge = ClientBridge::new();
        bridge.bind(Arc::new(Silent));
        let approver = BridgeApprover::new(Arc::clone(&bridge));
        let answer = approver.approve(&request()).await;
        assert!(!answer.allow);
        assert!(!answer.remember);
    }

    #[test]
    fn a_declared_tool_is_registrable_and_a_broken_one_is_named() {
        let bridge = ClientBridge::new();
        let (tools, rejected) = dynamic_tools(
            &bridge,
            &[
                DynamicToolSpec {
                    name: "lookup".into(),
                    description: "Looks a record up in the client".into(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": { "id": { "type": "string" } },
                        "required": ["id"],
                        "additionalProperties": false
                    }),
                },
                DynamicToolSpec {
                    name: "bad name".into(),
                    description: "d".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                DynamicToolSpec {
                    name: "lookup".into(),
                    description: "duplicate".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ],
        );
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "lookup");
        // Fail-closed metadata is not negotiable by the client.
        assert!(tools[0].is_sensitive());
        assert!(tools[0].returns_untrusted());
        assert!(!tools[0].is_read_only());
        assert_eq!(rejected.len(), 2);
        assert!(rejected[0].reason.contains("A-Za-z0-9_-"));
        assert!(rejected[1].reason.contains("already declared"));
    }
}
