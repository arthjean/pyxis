//! Model-visible Code Mode tools: `exec` (freeform) and `wait` (function).
//!
//! Both are thin: the cell state machine lives in `agent-code-mode` and the
//! isolate in `agent-code-mode-v8`. What they add is the only thing the
//! Registry can give, which is the tool pipeline itself. `exec` therefore has
//! NO effect of its own: everything a cell does goes back through this same
//! Registry as a nested call, with its own permission, taint, hooks and
//! cancellation (`agent_code_mode::PlanDispatcher`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_code_mode::nested::{NestedLoopGuard, NestedToolBinding, NestedToolDispatcher};
use agent_code_mode::protocol::{
    CellId, CodeModeError, ExecuteRequest, NestedTool, OutputItem, RuntimeResponse, SessionId,
    ShutdownReport, WaitRequest,
};
use agent_code_mode::session::CodeModeSession;
use agent_code_mode::tools::{
    EXEC_GRAMMAR, EXEC_TOOL_NAME, ExecInputError, WAIT_TOOL_NAME, exec_tool_spec,
    parse_exec_source, wait_tool_spec,
};
use agent_core::provider::{GrammarSyntax, ToolGrammar, ToolKind};
use agent_core::sync::lock;
use agent_core::tools::ToolImage;
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Conversion used when a cell asks for a token budget: the workspace heuristic
/// is ~1 token per 4 UTF-8 bytes (`agent_tokenizer::HeuristicCounter`), and the
/// session bounds bytes. Using the same ratio keeps the two consistent instead
/// of inventing a second estimate.
const BYTES_PER_TOKEN: usize = 4;

/// Budget for closing a session when the thread changes or the run ends. Same
/// order as the runtime's own shutdown deadline: a conversation must never hang
/// on a cell that refuses to stop.
const SESSION_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// Opens a Code Mode session. Implemented by the binary that owns the
/// JavaScript engine, so this crate never sees V8.
pub trait CodeModeSessionFactory: Send + Sync {
    fn open(&self, id: SessionId) -> Result<Arc<CodeModeSession>, String>;
}

/// Everything the two tools share: the session of the CURRENT thread, the
/// pipeline a cell dispatches through, and the catalog the model is shown.
///
/// Two things move here, at two different rhythms. `bind_thread` follows the
/// conversation: a new or resumed thread gets a NEW session, and the previous
/// one is shut down instead of being left running, which is what makes a
/// resumed thread restart with an empty JavaScript session. `bind_step` follows
/// the model request: the dispatcher and the catalog are those of the step that
/// captured them, so a stale tool plan cannot survive into the next step.
pub struct CodeModeHandle {
    factory: Arc<dyn CodeModeSessionFactory>,
    session: Mutex<Option<Arc<CodeModeSession>>>,
    binding: NestedToolBinding,
    catalog: Mutex<Vec<NestedTool>>,
    code_mode_only: AtomicBool,
    loop_guard: NestedLoopGuard,
}

impl CodeModeHandle {
    pub fn new(factory: Arc<dyn CodeModeSessionFactory>, binding: NestedToolBinding) -> Self {
        Self {
            factory,
            session: Mutex::new(None),
            binding,
            catalog: Mutex::new(Vec::new()),
            code_mode_only: AtomicBool::new(false),
            loop_guard: NestedLoopGuard::default(),
        }
    }

    /// Attaches the handle to `thread`. The session of the previous thread is
    /// shut down first: every open cell reaches a terminal state, and no cell
    /// keeps running for a conversation the user has left.
    pub async fn bind_thread(&self, thread: &str) -> Result<(), String> {
        // Taken in a statement of its own: a guard held across the await would
        // make every caller's future `!Send`, and the app-server drives this
        // from a spawned task.
        let previous = lock(&self.session).take();
        if let Some(previous) = previous {
            previous.shutdown(SESSION_SHUTDOWN_DEADLINE).await;
        }
        let session = self.factory.open(SessionId::new(thread))?;
        self.loop_guard.reset();
        *lock(&self.session) = Some(session);
        Ok(())
    }

    /// Closes the current session. Called when the run ends.
    pub async fn shutdown(&self) -> Option<ShutdownReport> {
        let session = lock(&self.session).take()?;
        self.binding.clear();
        Some(session.shutdown(SESSION_SHUTDOWN_DEADLINE).await)
    }

    /// Binds the tool plan of the step in flight and refreshes what the model
    /// is told it can call from JavaScript.
    pub fn bind_step(&self, dispatcher: Arc<dyn NestedToolDispatcher>, code_mode_only: bool) {
        // Projected as a WHOLE set: `catalog` is what allocates a unique
        // JavaScript binding per tool, which a name-by-name projection cannot
        // promise.
        *lock(&self.catalog) = NestedTool::catalog(&dispatcher.catalog());
        self.code_mode_only.store(code_mode_only, Ordering::Release);
        self.binding.set(dispatcher);
    }

    fn bind_events(&self, events: agent_core::tools::ToolEventSink) {
        if let Some(dispatcher) = self.binding.get() {
            dispatcher.bind_events(events);
        }
    }

    /// Session of the current thread. `None` before `bind_thread`.
    pub fn session(&self) -> Option<Arc<CodeModeSession>> {
        lock(&self.session).clone()
    }

    pub fn catalog(&self) -> Vec<NestedTool> {
        lock(&self.catalog).clone()
    }

    pub fn loop_guard(&self) -> NestedLoopGuard {
        self.loop_guard.clone()
    }

    pub fn begin_turn(&self) {
        self.loop_guard.reset();
    }

    fn finish_cell_if_terminal(&self, response: &RuntimeResponse) {
        if response.state().is_terminal() {
            self.loop_guard.finish_cell(response.cell_id());
        }
    }

    fn code_mode_only(&self) -> bool {
        self.code_mode_only.load(Ordering::Acquire)
    }

    /// A missing session is a NAMED refusal, never a panic: it is exactly edge
    /// case 1 of the PRD, a model that requires Code Mode without a runtime.
    fn require_session(&self) -> Result<Arc<CodeModeSession>, ToolError> {
        self.session().ok_or_else(|| {
            ToolError::Rejected(
                "no Code Mode session is attached to this thread: the runtime is not available"
                    .into(),
            )
        })
    }
}

/// `exec`: runs JavaScript in the thread's Code Mode session.
pub struct ExecTool {
    handle: Arc<CodeModeHandle>,
}

impl ExecTool {
    pub fn new(handle: Arc<CodeModeHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for ExecTool {
    /// Raw JavaScript. A freeform call carries its text as a JSON string, so
    /// the pipeline parses it like any other input without a second path.
    type Input = String;

    fn name(&self) -> &str {
        EXEC_TOOL_NAME
    }

    fn description(&self) -> String {
        exec_tool_spec(&self.handle.catalog(), self.handle.code_mode_only()).description
    }

    /// Never exposed: `tool_kind` says freeform, so the spec carries the
    /// grammar and no schema at all.
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [],
            "properties": {}
        })
    }

    fn tool_kind(&self) -> ToolKind {
        ToolKind::Freeform {
            grammar: Some(ToolGrammar {
                syntax: GrammarSyntax::Lark,
                definition: EXEC_GRAMMAR.to_string(),
            }),
        }
    }

    /// A malformed pragma or an empty source is refused BEFORE the permission
    /// decision, so a cell is never created for an input the model has to fix.
    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        parse_exec_source(input)
            .map(|_| ())
            .map_err(|error: ExecInputError| ValidationError::new(error.to_string()))
    }

    fn is_read_only(&self) -> bool {
        false
    }

    /// `exec` itself performs nothing: a cell has no file system, no network
    /// and no host binding other than the helpers. Every effect it can reach is
    /// a nested call that meets its OWN permission and taint decision at
    /// dispatch time. Gating `exec` too would ask twice for one action, and
    /// would make a pure computation cell impossible after any untrusted read.
    fn is_sensitive(&self) -> bool {
        false
    }

    fn is_taint_sensitive(&self) -> bool {
        false
    }

    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.handle.bind_events(ctx.events.clone());
        let (pragma, source) =
            parse_exec_source(&input).map_err(|error| ToolError::Parse(error.to_string()))?;
        let mut request =
            ExecuteRequest::new(EXEC_TOOL_NAME, source).with_tools(self.handle.catalog());
        if let Some(yield_time_ms) = pragma.yield_time_ms {
            request = request.with_yield_time(Duration::from_millis(yield_time_ms));
        }
        if let Some(max_output_tokens) = pragma.max_output_tokens {
            request =
                request.with_max_output_bytes(max_output_tokens.saturating_mul(BYTES_PER_TOKEN));
        }
        let response = self
            .handle
            .require_session()?
            .execute(request)
            .await
            .map_err(session_error)?;
        self.handle.finish_cell_if_terminal(&response);
        Ok(render(response))
    }
}

/// `wait`: resumes or stops a yielded cell.
pub struct WaitTool {
    handle: Arc<CodeModeHandle>,
}

impl WaitTool {
    pub fn new(handle: Arc<CodeModeHandle>) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {
    pub cell_id: String,
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[serde(default)]
    pub terminate: Option<bool>,
}

#[async_trait]
impl Tool for WaitTool {
    type Input = WaitInput;

    fn name(&self) -> &str {
        WAIT_TOOL_NAME
    }

    fn description(&self) -> String {
        wait_tool_spec().description
    }

    fn input_schema(&self) -> serde_json::Value {
        wait_tool_spec()
            .input_schema()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" }))
    }

    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        if CellId::parse(input.cell_id.clone()).is_none() {
            return Err(ValidationError::new("cell_id must not be empty"));
        }
        Ok(())
    }

    /// Same reasoning as `exec`: waiting on a cell of THIS thread reaches
    /// nothing a nested call has not already been authorized to reach.
    fn is_sensitive(&self) -> bool {
        false
    }

    fn is_taint_sensitive(&self) -> bool {
        false
    }

    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.handle.bind_events(ctx.events.clone());
        let Some(cell_id) = CellId::parse(input.cell_id) else {
            return Err(ToolError::Validation(ValidationError::new(
                "cell_id must not be empty",
            )));
        };
        let mut request = WaitRequest::new(cell_id);
        request.terminate = input.terminate.unwrap_or(false);
        if let Some(yield_time_ms) = input.yield_time_ms {
            request = request.with_yield_time(Duration::from_millis(yield_time_ms));
        }
        let response = self
            .handle
            .require_session()?
            .wait(request)
            .await
            .map_err(session_error)?;
        self.handle.finish_cell_if_terminal(&response);
        Ok(render(response))
    }
}

/// A session refusal is a SEMANTIC error for the model: the call was well
/// formed, the cell it named is not usable. It never becomes a panic and never
/// becomes a generic failure whose cause the model has to guess.
fn session_error(error: CodeModeError) -> ToolError {
    match error {
        CodeModeError::UnknownCell { .. } | CodeModeError::ForeignCell { .. } => {
            ToolError::Validation(ValidationError::new(error.to_string()))
        }
        other => ToolError::Rejected(other.to_string()),
    }
}

/// Projects a runtime response onto the tool result the model reads.
fn render(response: RuntimeResponse) -> ToolOutput {
    let cell_id = response.cell_id().clone();
    let state = response.state();
    let failure = response.failure().cloned();
    let (items, omitted) = match &response {
        RuntimeResponse::Yielded {
            items,
            omitted_bytes,
            ..
        }
        | RuntimeResponse::Terminated {
            items,
            omitted_bytes,
            ..
        }
        | RuntimeResponse::Finished {
            items,
            omitted_bytes,
            ..
        } => (items.clone(), *omitted_bytes),
    };

    let mut lines = Vec::new();
    let mut images = Vec::new();
    for item in items {
        match item {
            OutputItem::Text { text } => lines.push(text),
            OutputItem::Image { image_url, .. } => match split_data_url(&image_url) {
                Some((media_type, data)) => images.push(ToolImage { media_type, data }),
                // An image the transport cannot carry is NAMED rather than
                // dropped: the model must not believe it was delivered.
                None => lines.push("[image ignorée: `image_url` n'est pas une data URL]".into()),
            },
            OutputItem::Audio { .. } => {
                lines.push("[audio produit par la cellule, non transporté par ce résultat]".into())
            }
        }
    }
    if omitted > 0 {
        lines.push(format!(
            "[{omitted} octets omis: plafond de résultat de cellule atteint]"
        ));
    }
    // The state line is what tells the model whether to call `wait`, so it is
    // always present, even for a cell that produced nothing.
    lines.push(match state {
        agent_code_mode::protocol::CellState::Yielded => format!(
            "Script running with cell ID {cell_id}. Call `{WAIT_TOOL_NAME}` with this cell_id to resume."
        ),
        other => format!("Cell {cell_id} {}.", other.label()),
    });
    if let Some(failure) = &failure {
        lines.push(format!("{}: {}", failure.kind.label(), failure.message));
    }

    let mut output = ToolOutput::text(lines.join("\n"));
    output.is_error = failure.is_some();
    output.images = images;
    output
}

/// `data:<media type>;base64,<payload>` into its two halves.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    (!media_type.is_empty() && !data.is_empty()).then(|| (media_type.to_string(), data.to_string()))
}

#[cfg(test)]
#[path = "code_mode_tests.rs"]
mod tests;
