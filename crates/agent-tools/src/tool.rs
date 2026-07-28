//! `Tool` trait (fail-closed) + object-safe `DynTool` + adapter (ARCHITECTURE
//! 4.1). The generic trait carries an associated input type (not object-safe);
//! `DynTool` is the dyn-compatible wrapper stored in the `Registry`. From the
//! dispatch point of view, a native tool and (eventually) an MCP tool are
//! indistinguishable.
//!
//! FAIL-CLOSED defaults (invariant 4): without an explicit override, a tool is
//! assumed non-concurrent, mutating, sensitive, and with untrusted output.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::event::PlanView;
use agent_core::provider::ToolKind;
use agent_core::sandbox::SandboxPolicy;
use agent_core::tools::{ToolExecution, ToolImage};
use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::error::{ToolError, ValidationError};
use crate::permission::{ApprovalMemo, PermCtx, PermissionDecision};
use crate::sandbox::{SandboxDenial, SandboxObserver};

/// Global caps of the native tools. These limits bound allocations before
/// a model payload can become a memory or disk problem.
pub const MAX_TOOL_INPUT_BYTES: usize = 4_000_000;
/// Cap of the text a tool returns to the model. Shared by `bash` and by the MCP
/// tools so one truncation policy covers every tool output.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 30_000;
pub const MAX_WRITE_BYTES: usize = 2_000_000;
pub const MAX_EDIT_FILE_BYTES: u64 = 5_000_000;
pub const MAX_EDIT_ANCHOR_BYTES: usize = 200_000;
pub const MAX_COMMAND_BYTES: usize = 16_000;

/// Opaque hardening of a shell command (Bash): closure injected by
/// agent-cli, which applies the network sandbox (`HTTP_PROXY` env). Opaque here
/// to keep `agent-tools` decoupled from `agent-sandbox`; the Landlock FS
/// confinement is process-wide (inherited), hence transparent to the tools.
pub type CommandHardener = Arc<dyn Fn(&mut tokio::process::Command) + Send + Sync>;

/// Emitter of output fragments for a running tool (US-015). Installed by the
/// Registry for the duration of a call, already correlated to that call's `id`:
/// a tool therefore knows nothing about the transport nor about its own identifier.
pub type OutputSink = Arc<dyn Fn(String) + Send + Sync>;

/// Shared execution context passed to every tool. `&ToolCtx` (shared):
/// concurrent tools read it in parallel. Agent state mutation
/// (context-modifiers) is deferred (Phase 2).
#[derive(Clone)]
pub struct ToolCtx {
    /// Workspace root: anchor of relative paths and confinement boundary
    /// (enforced at kernel level by process-wide Landlock, US-020).
    pub workspace: PathBuf,
    /// Confinement perimeter in force (US-001). Evaluated BEFORE execution,
    /// which is the only level able to subtract a subpath from a root the
    /// kernel has already opened (US-002).
    pub sandbox: SandboxPolicy,
    /// Is the kernel confinement really applied? Distinguishes a refusal caused
    /// by the sandbox from an ordinary permission problem (US-004).
    pub sandbox_enforced: bool,
    /// What the confinement blocked during a call (US-004). Injected by
    /// agent-cli over the network proxy; `None` outside the binary.
    pub sandbox_observer: Option<Arc<dyn SandboxObserver>>,
    /// Timeout applied by the Registry around `call()`.
    pub timeout: Duration,
    /// Grace given to tools that must clean up after their internal timeout.
    pub cleanup_grace: Duration,
    /// Does the active model read images (US-011)? Comes from the provider
    /// capabilities. Fail-closed default: `false`, so a caller that does not
    /// declare vision never gets an image sent on its behalf.
    pub vision: bool,
    /// Persistent shell sessions of the run (US-012). Shared by `exec_command`
    /// and `write_stdin`; empty and inert as long as neither is called.
    pub sessions: crate::exec_session::ExecSessions,
    /// Command hardening (Bash network sandbox), injected by agent-cli.
    pub harden: Option<CommandHardener>,
    /// Progressive output of the current call (US-015). `None` = no
    /// consumer: tools then emit nothing.
    pub output: Option<OutputSink>,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("workspace", &self.workspace)
            .field("sandbox", &self.sandbox.id())
            .field("sandbox_enforced", &self.sandbox_enforced)
            .field("timeout", &self.timeout)
            .field("cleanup_grace", &self.cleanup_grace)
            .field("harden", &self.harden.as_ref().map(|_| "<fn>"))
            .field("output", &self.output.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl ToolCtx {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            // Default policy = what Pyxis did before US-001: writes confined to
            // the workspace, with the deferred-execution subpaths subtracted.
            sandbox: SandboxPolicy::workspace_write(
                workspace.clone(),
                Vec::new(),
                crate::path::PROTECTED_SUBPATHS.iter().map(PathBuf::from),
            ),
            sandbox_enforced: false,
            sandbox_observer: None,
            workspace,
            timeout: Duration::from_secs(120),
            cleanup_grace: Duration::from_secs(2),
            vision: false,
            sessions: crate::exec_session::ExecSessions::new(),
            harden: None,
            output: None,
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn with_hardener(mut self, harden: CommandHardener) -> Self {
        self.harden = Some(harden);
        self
    }
    /// Declares that the active model reads images (US-011).
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }
    /// Confinement perimeter in force for the session (US-001).
    pub fn with_sandbox(mut self, policy: SandboxPolicy, enforced: bool) -> Self {
        self.sandbox = policy;
        self.sandbox_enforced = enforced;
        self
    }
    pub fn with_sandbox_observer(mut self, observer: Arc<dyn SandboxObserver>) -> Self {
        self.sandbox_observer = Some(observer);
        self
    }
    /// Context derived for ONE call, equipped with its output emitter (US-015).
    pub fn with_output_sink(&self, output: OutputSink) -> Self {
        let mut ctx = self.clone();
        ctx.output = Some(output);
        ctx
    }
    /// Publishes an output fragment when a consumer is listening. No-op otherwise.
    pub fn emit_output(&self, chunk: impl Into<String>) {
        if let Some(sink) = &self.output {
            sink(chunk.into());
        }
    }
}

/// Output of a tool: the text the model will see as `tool_result`.
/// `is_error` distinguishes a *semantic* failure (Bash command exiting non-zero)
/// from a real pipeline error (`ToolError`); in both cases the model sees the
/// content and can react.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    /// Protocol-native payload retained alongside the text fallback. The core
    /// decides whether it fits the active model feedback budget atomically.
    pub structured_content: Option<serde_json::Value>,
    pub is_error: bool,
    /// Set when the tool attributes its failure to the confinement (US-004).
    /// The Registry, which owns the approver, is what turns it into an
    /// escalation offer; the tool only states the cause.
    pub denial: Option<SandboxDenial>,
    /// Structured plan the call published (US-009). Addressed to the CLIENT,
    /// not to the model: the Registry forwards it as a dispatch event, next to
    /// the textual result the model reads.
    pub plan: Option<PlanView>,
    /// Images the call brought into the turn (US-011), forwarded by the
    /// Registry into the outcome so the loop can turn them into content blocks.
    pub images: Vec<ToolImage>,
    /// Terminal shell facts, kept outside display text so later truncation can
    /// preserve them.
    pub execution: Option<ToolExecution>,
}

impl ToolOutput {
    /// Nominal output (success).
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            structured_content: None,
            is_error: false,
            denial: None,
            plan: None,
            images: Vec::new(),
            execution: None,
        }
    }
    /// Output marked as a semantic error (the content is kept for the model).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            structured_content: None,
            is_error: true,
            denial: None,
            plan: None,
            images: Vec::new(),
            execution: None,
        }
    }
    /// Attributes the failure to the confinement (US-004 AC1).
    pub fn with_denial(mut self, denial: SandboxDenial) -> Self {
        self.denial = Some(denial);
        self
    }
    /// Publishes a plan alongside the textual result (US-009).
    pub fn with_plan(mut self, plan: PlanView) -> Self {
        self.plan = Some(plan);
        self
    }
    /// Brings images into the turn (US-011).
    pub fn with_images(mut self, images: Vec<ToolImage>) -> Self {
        self.images = images;
        self
    }
    pub fn with_structured_content(mut self, content: serde_json::Value) -> Self {
        self.structured_content = Some(content);
        self
    }
    pub fn with_execution(mut self, execution: ToolExecution) -> Self {
        self.execution = Some(execution);
        self
    }
}

/// Trait of native tools. Generic over the input -> monomorphized, plugged into the
/// Registry through `DynToolAdapter`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Input type (deserialized from the `tool_use` JSON).
    type Input: DeserializeOwned + Send;

    fn name(&self) -> &str;
    /// Description given to the model (capped by the Registry at exposure time).
    fn description(&self) -> String;
    /// Input JSON Schema (exposed to the model in `ToolSpec`).
    fn input_schema(&self) -> serde_json::Value;

    /// How the model addresses this tool. The default is the function shape
    /// every native tool has had so far; a tool whose input is TEXT overrides
    /// it and its `input_schema` is then never exposed, which is what stops an
    /// adapter from inventing a schema for a freeform tool (US-002).
    fn tool_kind(&self) -> ToolKind {
        ToolKind::Function {
            input_schema: self.input_schema(),
        }
    }

    // ───── FAIL-CLOSED defaults (invariant 4): a tool widens them explicitly.
    /// Can run alongside other tools (typically reads).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    /// Performs no mutation (pure read).
    fn is_read_only(&self) -> bool {
        false
    }
    /// Destructive or network action -> target of the taint defense (4.6): when
    /// taint is recent, we force `Ask` even in a permissive mode.
    fn is_sensitive(&self) -> bool {
        true
    }
    /// Must a recent untrusted output force a confirmation before this
    /// tool? By default, every mutation or sensitive action is protected.
    fn is_taint_sensitive(&self) -> bool {
        self.is_sensitive() || !self.is_read_only()
    }
    /// Untrusted (tainted) output: the default for any tool output (OWASP
    /// LLM01).
    fn returns_untrusted(&self) -> bool {
        true
    }

    /// Behavioral invariants colocated with the tool (US-026): rules the model
    /// must know to use it well (e.g. "the anchor is searched in the original
    /// file"). Collected by the Registry and injected into the system prompt.
    /// Default: none.
    fn behavioral_guidelines(&self) -> &[&'static str] {
        &[]
    }

    /// Input validation (pre-permission, pre-execution). Default: accepts. The
    /// `ToolCtx` is provided for rules that depend on the workspace: protecting
    /// deferred-execution subpaths (US-013) is one of them. Refused here, it
    /// precedes the permission decision and therefore cannot be lifted by a
    /// permissive mode.
    fn validate_input(&self, _input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        Ok(())
    }

    /// The tool's own *baseline* decision. Fail-closed default: `Ask`.
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    /// Key identifying this call for the session approval memory (US-008).
    /// Fail-closed default: no answer is ever remembered for this tool.
    fn approval_memo(&self, _input: &Self::Input) -> ApprovalMemo {
        ApprovalMemo::NotApplicable
    }

    /// External timeout applied by the Registry. Tools that manage an internal
    /// timeout, such as `bash`, can ask for grace to clean up.
    fn timeout(&self, ctx: &ToolCtx) -> Duration {
        ctx.timeout
    }

    /// Execution. The Registry already wraps it in a `timeout`: a `call` that
    /// hangs does not block the loop.
    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Object-safe facade stored in the Registry. Raw JSON travels through here; the
/// parse into `Tool::Input` is internal to the adapter.
#[async_trait]
pub trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn input_schema(&self) -> serde_json::Value;
    /// Function by default, so every existing implementor keeps its shape.
    fn kind(&self) -> ToolKind {
        ToolKind::Function {
            input_schema: self.input_schema(),
        }
    }
    fn is_concurrency_safe(&self) -> bool;
    fn is_read_only(&self) -> bool;
    fn is_sensitive(&self) -> bool;
    fn is_taint_sensitive(&self) -> bool;
    fn returns_untrusted(&self) -> bool;
    /// Behavioral invariants of the tool (US-026), forwarded from `Tool`.
    fn behavioral_guidelines(&self) -> &[&'static str];
    /// Parse + `validate_input` WITHOUT executing (fail-closed, US-010 AC3). An error
    /// means the Registry returns the failure to the agent without calling `call`.
    fn precheck(&self, raw: &serde_json::Value, ctx: &ToolCtx) -> Result<(), ToolError>;
    /// Baseline decision of the tool (raw already validated by `precheck`).
    fn permission(&self, raw: &serde_json::Value, ctx: &PermCtx) -> PermissionDecision;
    /// Session approval key of this call (US-008), forwarded from `Tool`.
    fn approval_memo(&self, raw: &serde_json::Value) -> ApprovalMemo;
    fn timeout(&self, ctx: &ToolCtx) -> Duration;
    /// Parse + `call`. Wrapped in a timeout by the Registry.
    async fn invoke(&self, raw: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Generic `Tool` -> `DynTool` adapter.
pub struct DynToolAdapter<T: Tool> {
    inner: T,
}

impl<T: Tool> DynToolAdapter<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<T: Tool> DynTool for DynToolAdapter<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> String {
        self.inner.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }
    fn kind(&self) -> ToolKind {
        self.inner.tool_kind()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn is_sensitive(&self) -> bool {
        self.inner.is_sensitive()
    }
    fn is_taint_sensitive(&self) -> bool {
        self.inner.is_taint_sensitive()
    }
    fn returns_untrusted(&self) -> bool {
        self.inner.returns_untrusted()
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        self.inner.behavioral_guidelines()
    }
    fn precheck(&self, raw: &serde_json::Value, ctx: &ToolCtx) -> Result<(), ToolError> {
        let estimated = estimate_json_bytes(raw);
        if estimated > MAX_TOOL_INPUT_BYTES {
            return Err(ToolError::Validation(ValidationError::new(format!(
                "tool input too large: estimated {estimated} bytes > {MAX_TOOL_INPUT_BYTES}"
            ))));
        }
        let input: T::Input =
            serde_json::from_value(raw.clone()).map_err(|e| ToolError::Parse(e.to_string()))?;
        self.inner.validate_input(&input, ctx)?;
        Ok(())
    }
    fn permission(&self, raw: &serde_json::Value, ctx: &PermCtx) -> PermissionDecision {
        // `precheck` already guaranteed that the parse succeeds; on an unlikely
        // race, fail-closed -> Deny.
        match serde_json::from_value::<T::Input>(raw.clone()) {
            Ok(input) => self.inner.permission(&input, ctx),
            Err(_) => PermissionDecision::Deny,
        }
    }
    fn approval_memo(&self, raw: &serde_json::Value) -> ApprovalMemo {
        // Same race, same posture: an unparsable input is not rememberable.
        match serde_json::from_value::<T::Input>(raw.clone()) {
            Ok(input) => self.inner.approval_memo(&input),
            Err(_) => ApprovalMemo::NotApplicable,
        }
    }
    fn timeout(&self, ctx: &ToolCtx) -> Duration {
        self.inner.timeout(ctx)
    }
    async fn invoke(&self, raw: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let input: T::Input =
            serde_json::from_value(raw).map_err(|e| ToolError::Parse(e.to_string()))?;
        self.inner.call(input, ctx).await
    }
}

/// Boxes a native tool into a `DynTool` ready for the Registry.
pub fn into_dyn<T: Tool + 'static>(tool: T) -> Box<dyn DynTool> {
    Box::new(DynToolAdapter::new(tool))
}

/// Truncates `body` keeping the TAIL within `max` bytes (US-026): on a long
/// output (compilation: warnings first, errors + exit code last), the tail
/// preserves the critical information. The cut point is aligned on a UTF-8
/// character boundary (never an indexing panic).
pub fn truncate_tail(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let mut cut = body.len() - max;
    while cut < body.len() && !body.is_char_boundary(cut) {
        cut += 1;
    }
    format!(
        "[... output truncated, {cut} bytes, beginning omitted]\n{}",
        &body[cut..]
    )
}

/// Cheap size estimate of a JSON payload, without serializing it. Public so a
/// `DynTool` implemented outside this crate (MCP tools) can apply the same
/// input bound as `DynToolAdapter`.
pub fn estimate_json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(items) => items.iter().map(estimate_json_bytes).sum(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| key.len() + estimate_json_bytes(value))
            .sum(),
    }
}
