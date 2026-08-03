//! Tool dispatch contract and the immutable plan used by one model sampling.
//! The real implementation (registry, permissions, taint, pipeline) is
//! `agent-tools`; the core only owns the canonical plan/result vocabulary.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use agent_tokenizer::TokenCounter;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::event::{PermissionReq, PlanView};
use crate::message::{ToolCallFormat, ToolCallId, ToolErrorKind, is_false};
use crate::provider::ToolSpec;

/// Absolute ceiling for one model-visible tool result, independently of the
/// model profile. This bounds persisted and provider-bound allocations.
pub const MAX_MODEL_TOOL_RESULT_BYTES: usize = 64 * 1024;
/// Smallest valid profile budget when terminal execution metadata may need to
/// be rendered without losing its status fields.
pub const MIN_MODEL_TOOL_RESULT_BYTES: usize = 256;
pub const MIN_MODEL_TOOL_RESULT_TOKENS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Error,
    Rejected,
    Cancelled,
    TimedOut,
    SandboxDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStrategy {
    Head,
    Tail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultTruncation {
    pub original_bytes: usize,
    pub kept_bytes: usize,
    pub strategy: TruncationStrategy,
    pub continuation_hint: String,
}

/// Terminal facts of a shell-like execution. They are separate from display
/// text so truncation cannot accidentally erase the only proof of completion.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolExecution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub session_closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// A tool call requested by the model (args already reassembled into valid JSON).
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: ToolCallId,
    pub name: String,
    /// JSON arguments for a `Json` call, the raw text for a `Text` one.
    pub input: serde_json::Value,
    /// Carried to dispatch so a freeform call is never re-read as JSON.
    pub format: ToolCallFormat,
}

impl ToolInvocation {
    pub fn json(
        id: impl Into<ToolCallId>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            format: ToolCallFormat::Json,
        }
    }

    /// Text input of a freeform call, as the model emitted it.
    pub fn text_input(&self) -> Option<&str> {
        self.format.is_text().then(|| self.input.as_str()).flatten()
    }
}

/// Authoritative result of one tool call. The model payload, canonical
/// transcript and client event are all projections of this value.
#[derive(Debug, Clone)]
pub struct ModelToolResult {
    pub id: ToolCallId,
    pub content: String,
    pub structured_content: Option<serde_json::Value>,
    pub status: ToolResultStatus,
    pub is_error: bool,
    pub untrusted: bool,
    pub error_kind: Option<ToolErrorKind>,
    pub duration_ms: Option<u64>,
    pub truncation: Option<ToolResultTruncation>,
    pub execution: Option<ToolExecution>,
    /// Images the tool brought into the turn (US-011). A `tool_result` block
    /// carries text only, so the loop turns these into a user message right
    /// after the result. Empty for every other tool.
    pub images: Vec<ToolImage>,
    /// The user refused this call AND ended the turn with it. The result is
    /// still written to the transcript (the backend needs its pair), but the
    /// loop stops instead of sampling the model again. Mirrors Codex
    /// `ReviewDecision::Abort` (`codex-rs/protocol/src/protocol.rs:4138`).
    pub aborts_turn: bool,
    /// The tool asked for a fresh context window (`new_context_window`). It is a
    /// REQUEST, not an order: it arms the same mid-turn compaction a long
    /// `tool_result` arms (US-030), so the loop keeps deciding when and how to
    /// compact, and a model cannot use it to wipe a transcript on demand.
    pub requests_compaction: bool,
}

impl ModelToolResult {
    pub fn new(
        id: ToolCallId,
        content: String,
        is_error: bool,
        untrusted: bool,
        error_kind: Option<ToolErrorKind>,
    ) -> Self {
        let status = status_from_error(is_error, error_kind);
        Self {
            id,
            content,
            structured_content: None,
            status,
            is_error,
            untrusted,
            error_kind,
            duration_ms: None,
            truncation: None,
            execution: None,
            images: Vec::new(),
            aborts_turn: false,
            requests_compaction: false,
        }
    }

    pub fn rejected(id: ToolCallId, content: impl Into<String>, kind: ToolErrorKind) -> Self {
        let mut result = Self::new(id, content.into(), true, false, Some(kind));
        result.status = ToolResultStatus::Rejected;
        result
    }

    pub fn cancelled(id: ToolCallId, content: impl Into<String>) -> Self {
        let mut result = Self::new(
            id,
            content.into(),
            true,
            false,
            Some(ToolErrorKind::Cancelled),
        );
        result.status = ToolResultStatus::Cancelled;
        result.execution = Some(ToolExecution {
            cancelled: true,
            ..ToolExecution::default()
        });
        result
    }

    pub fn with_duration(mut self, duration: std::time::Duration) -> Self {
        self.duration_ms = Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        self
    }

    /// Produces the one bounded string sent to the model while retaining the
    /// typed source fields for resume and clients. Structured JSON is included
    /// whole or omitted whole, never cut into invalid JSON.
    pub fn bound_feedback(
        mut self,
        counter: &dyn TokenCounter,
        token_limit: usize,
        byte_limit: usize,
    ) -> Self {
        let carries_terminal = self.execution.is_some();
        let byte_limit = byte_limit
            .min(MAX_MODEL_TOOL_RESULT_BYTES)
            .max(if carries_terminal {
                MIN_MODEL_TOOL_RESULT_BYTES
            } else {
                1
            });
        let token_limit = token_limit.max(if carries_terminal {
            MIN_MODEL_TOOL_RESULT_TOKENS
        } else {
            1
        });
        let original_text = self.content.clone();
        let mut structured = self
            .structured_content
            .as_ref()
            .and_then(|value| serde_json::to_string(value).ok());
        let execution = self.execution.as_ref().map(|execution| {
            render_execution(
                execution,
                self.duration_ms,
                counter,
                token_limit,
                byte_limit,
            )
        });
        let original = render_feedback(&original_text, structured.as_deref(), execution.as_deref());
        let original_bytes = original.len();
        if feedback_fits(&original, counter, token_limit, byte_limit) {
            self.content = original;
            return self;
        }

        let strategy = if self.execution.is_some() {
            TruncationStrategy::Tail
        } else {
            TruncationStrategy::Head
        };
        let hint = "Re-run the tool with a narrower query or explicit range.".to_string();
        let marker = format!(
            "[tool output truncated; strategy={}; continuation={hint}]",
            match strategy {
                TruncationStrategy::Head => "head",
                TruncationStrategy::Tail => "tail",
            }
        );

        // A structured result is useful only when it fits atomically beside the
        // terminal suffix and truncation marker. Otherwise the bounded text
        // fallback remains authoritative.
        if structured.as_ref().is_some_and(|json| {
            !feedback_fits(
                &render_feedback("", Some(json), execution.as_deref()),
                counter,
                token_limit,
                byte_limit,
            )
        }) {
            structured = None;
            if original_text.trim().is_empty() {
                self.status = ToolResultStatus::Error;
                self.is_error = true;
                self.error_kind = Some(ToolErrorKind::Semantic);
            }
        }

        let mut bounded_text = truncate_feedback_text(
            &original_text,
            structured.as_deref(),
            execution.as_deref(),
            &marker,
            strategy,
            counter,
            token_limit,
            byte_limit,
        );
        let mut rendered = render_feedback(
            &format!("{bounded_text}\n{marker}"),
            structured.as_deref(),
            execution.as_deref(),
        );
        // Never let the last-resort text cut split structured JSON. If the full
        // structure fits alone but not beside the marker, drop it atomically and
        // rebuild from the bounded text fallback.
        if structured.is_some() && !feedback_fits(&rendered, counter, token_limit, byte_limit) {
            bounded_text = truncate_feedback_text(
                &original_text,
                None,
                execution.as_deref(),
                &marker,
                strategy,
                counter,
                token_limit,
                byte_limit,
            );
            rendered = render_feedback(
                &format!("{bounded_text}\n{marker}"),
                None,
                execution.as_deref(),
            );
        }
        // Extremely small profile limits may not fit the descriptive marker.
        // The final cut still respects UTF-8 and the hard/token bounds.
        self.content = if feedback_fits(&rendered, counter, token_limit, byte_limit) {
            rendered
        } else if let Some(terminal) = execution {
            // Terminal facts are irreducible. They were bounded independently
            // above, so even the smallest valid profile retains completion.
            terminal
        } else {
            truncate_plain(&rendered, strategy, counter, token_limit, byte_limit)
        };
        self.truncation = Some(ToolResultTruncation {
            original_bytes,
            kept_bytes: self.content.len(),
            strategy,
            continuation_hint: hint,
        });
        self
    }
}

fn status_from_error(is_error: bool, error_kind: Option<ToolErrorKind>) -> ToolResultStatus {
    match error_kind {
        Some(ToolErrorKind::Timeout) => ToolResultStatus::TimedOut,
        Some(
            ToolErrorKind::UnknownTool
            | ToolErrorKind::Parse
            | ToolErrorKind::Validation
            | ToolErrorKind::OutsideWorkspace
            | ToolErrorKind::Rejected
            | ToolErrorKind::PermissionDenied,
        ) => ToolResultStatus::Rejected,
        Some(ToolErrorKind::SandboxDenied) => ToolResultStatus::SandboxDenied,
        Some(ToolErrorKind::Cancelled) => ToolResultStatus::Cancelled,
        Some(ToolErrorKind::Io | ToolErrorKind::Semantic) => ToolResultStatus::Error,
        None if is_error => ToolResultStatus::Error,
        None => ToolResultStatus::Success,
    }
}

fn render_execution(
    execution: &ToolExecution,
    duration_ms: Option<u64>,
    counter: &dyn TokenCounter,
    token_limit: usize,
    byte_limit: usize,
) -> String {
    let mut fields = Vec::new();
    if let Some(code) = execution.exit_code {
        fields.push(format!("exit_code={code}"));
    }
    if let Some(signal) = execution.signal {
        fields.push(format!("signal={signal}"));
    }
    if execution.timed_out {
        fields.push("timed_out=true".to_string());
    }
    if execution.cancelled {
        fields.push("cancelled=true".to_string());
    }
    if execution.session_closed {
        fields.push("session_closed=true".to_string());
    }
    if let Some(duration) = duration_ms {
        fields.push(format!("duration_ms={duration}"));
    }
    let core = format!("[execution {}]", fields.join(" "));
    let Some(stderr) = execution
        .stderr_tail
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return core;
    };
    let render = |tail: &str| {
        let mut fields = fields.clone();
        fields.push(format!(
            "stderr_tail={}",
            serde_json::Value::String(tail.to_string())
        ));
        format!("[execution {}]", fields.join(" "))
    };
    let full = render(stderr);
    if feedback_fits(&full, counter, token_limit, byte_limit) {
        return full;
    }
    let boundaries = char_boundaries(stderr);
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate = render(slice_chars(
            stderr,
            &boundaries,
            mid,
            TruncationStrategy::Tail,
        ));
        if feedback_fits(&candidate, counter, token_limit, byte_limit) {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    let bounded = render(slice_chars(
        stderr,
        &boundaries,
        low,
        TruncationStrategy::Tail,
    ));
    if feedback_fits(&bounded, counter, token_limit, byte_limit) {
        bounded
    } else {
        core
    }
}

fn render_feedback(text: &str, structured: Option<&str>, execution: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(json) = structured {
        parts.push(format!("[structured_content]\n{json}"));
    }
    if !text.is_empty() {
        parts.push(text.to_string());
    }
    if let Some(terminal) = execution {
        parts.push(terminal.to_string());
    }
    parts.join("\n")
}

fn feedback_fits(
    value: &str,
    counter: &dyn TokenCounter,
    token_limit: usize,
    byte_limit: usize,
) -> bool {
    value.len() <= byte_limit && counter.count_text(value) <= token_limit
}

#[allow(clippy::too_many_arguments)]
fn truncate_feedback_text(
    text: &str,
    structured: Option<&str>,
    execution: Option<&str>,
    marker: &str,
    strategy: TruncationStrategy,
    counter: &dyn TokenCounter,
    token_limit: usize,
    byte_limit: usize,
) -> String {
    let boundaries = char_boundaries(text);
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate = slice_chars(text, &boundaries, mid, strategy);
        let rendered = render_feedback(&format!("{candidate}\n{marker}"), structured, execution);
        if feedback_fits(&rendered, counter, token_limit, byte_limit) {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    slice_chars(text, &boundaries, low, strategy).to_string()
}

fn truncate_plain(
    text: &str,
    strategy: TruncationStrategy,
    counter: &dyn TokenCounter,
    token_limit: usize,
    byte_limit: usize,
) -> String {
    let boundaries = char_boundaries(text);
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate = slice_chars(text, &boundaries, mid, strategy);
        if feedback_fits(candidate, counter, token_limit, byte_limit) {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    slice_chars(text, &boundaries, low, strategy).to_string()
}

fn char_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    boundaries.push(text.len());
    boundaries
}

fn slice_chars<'a>(
    text: &'a str,
    boundaries: &[usize],
    chars: usize,
    strategy: TruncationStrategy,
) -> &'a str {
    match strategy {
        TruncationStrategy::Head => &text[..boundaries[chars]],
        TruncationStrategy::Tail => {
            let start = boundaries[boundaries.len().saturating_sub(chars + 1)];
            &text[start..]
        }
    }
}

/// Image a tool read for the model (US-011). Base64 payload plus its media
/// type: the exact pair `ContentBlock::Image` carries.
///
/// Serializable because it now travels INSIDE the tool result, so it has to
/// survive the transcript on disk like every other part of that result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub media_type: String,
    pub data: String,
}

/// Generation-bound dispatcher captured from the same registry read as the
/// visible specs. The trait object is intentionally opaque to the runtime.
#[derive(Clone)]
pub struct ToolDispatchSnapshot {
    generation: u64,
    specs: Vec<ToolSpec>,
    dispatcher: Arc<dyn ToolDispatch>,
}

impl ToolDispatchSnapshot {
    pub fn new(generation: u64, specs: Vec<ToolSpec>, dispatcher: Arc<dyn ToolDispatch>) -> Self {
        Self {
            generation,
            specs,
            dispatcher,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Same generation and same captured dispatcher, another visible catalog.
    ///
    /// Code Mode is what needs this: the model sees `exec` and `wait` while the
    /// nested calls dispatch against the full set. Both views share ONE
    /// dispatcher, so a tool hidden from the model still meets the same taint,
    /// hooks, permissions and cancellation it would meet directly.
    pub fn with_specs(&self, specs: Vec<ToolSpec>) -> Self {
        Self {
            generation: self.generation,
            specs,
            dispatcher: Arc::clone(&self.dispatcher),
        }
    }
}

impl fmt::Debug for ToolDispatchSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDispatchSnapshot")
            .field("generation", &self.generation)
            .field("specs", &self.specs)
            .finish_non_exhaustive()
    }
}

/// Complete immutable tool contract for one provider sampling.
#[derive(Clone)]
pub struct StepToolPlan {
    generation: u64,
    fingerprint: String,
    specs: Vec<ToolSpec>,
    parallel_allowed: bool,
    dispatcher: Arc<dyn ToolDispatch>,
}

impl fmt::Debug for StepToolPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StepToolPlan")
            .field("generation", &self.generation)
            .field("fingerprint", &self.fingerprint)
            .field("specs", &self.specs)
            .field("parallel_allowed", &self.parallel_allowed)
            .finish_non_exhaustive()
    }
}

impl StepToolPlan {
    pub fn capture(snapshot: ToolDispatchSnapshot, parallel_allowed: bool) -> Self {
        let ToolDispatchSnapshot {
            generation,
            specs,
            dispatcher,
        } = snapshot;
        let fingerprint = tool_plan_fingerprint(&specs, parallel_allowed);
        Self {
            generation,
            fingerprint,
            specs,
            parallel_allowed,
            dispatcher,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// The captured dispatcher, so the loop can ask it to qualify a call before
    /// dispatching it. Same generation as the specs the model saw.
    pub fn dispatcher(&self) -> &Arc<dyn ToolDispatch> {
        &self.dispatcher
    }

    pub fn with_parallel_allowed(&self, parallel_allowed: bool) -> Self {
        Self {
            generation: self.generation,
            fingerprint: tool_plan_fingerprint(&self.specs, parallel_allowed),
            specs: self.specs.clone(),
            parallel_allowed,
            dispatcher: Arc::clone(&self.dispatcher),
        }
    }

    /// Validates membership against the visible catalog before touching the
    /// captured dispatcher. Unknown calls receive one correlated rejection.
    pub async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        let visible: HashSet<&str> = self.specs.iter().map(|spec| spec.name.as_str()).collect();
        let order: Vec<ToolCallId> = calls.iter().map(|call| call.id.clone()).collect();
        let mut known = Vec::new();
        let mut outcomes = Vec::new();
        for call in calls {
            if visible.contains(call.name.as_str()) {
                known.push(call);
            } else {
                outcomes.push(ModelToolResult::rejected(
                    call.id,
                    format!("tool unavailable in this step: {}", call.name),
                    ToolErrorKind::UnknownTool,
                ));
            }
        }

        if self.parallel_allowed {
            outcomes.extend(self.dispatcher.dispatch(known, events).await);
        } else {
            for call in known {
                outcomes.extend(self.dispatcher.dispatch(vec![call], events.clone()).await);
            }
        }

        let mut by_id: HashMap<ToolCallId, ModelToolResult> = HashMap::new();
        let mut extras = Vec::new();
        for outcome in outcomes {
            if by_id.insert(outcome.id.clone(), outcome.clone()).is_some() {
                extras.push(outcome);
            }
        }
        let mut ordered = Vec::new();
        for id in order {
            if let Some(outcome) = by_id.remove(&id) {
                ordered.push(outcome);
            }
        }
        ordered.extend(by_id.into_values());
        ordered.extend(extras);
        ordered
    }
}

fn tool_plan_fingerprint(specs: &[ToolSpec], parallel_allowed: bool) -> String {
    let mut hasher = Sha256::new();
    hasher.update([u8::from(parallel_allowed)]);
    if let Ok(bytes) = serde_json::to_vec(specs) {
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone)]
pub enum ToolDispatchEvent {
    PermissionAsk(PermissionReq),
    /// A tool invoked from a Code Mode cell started. The outer `exec`/`wait`
    /// call is orchestration only; clients render this native call instead.
    NestedToolCall(crate::event::ToolCallView),
    /// Terminal result of a tool invoked from a Code Mode cell.
    NestedToolResult(crate::event::ToolResultView),
    /// Output fragment of a tool still running (US-015), correlated by `id`.
    /// Raw bytes and stream of origin, both preserved up to the client.
    OutputDelta {
        id: ToolCallId,
        stream: crate::event::OutputStream,
        chunk: Vec<u8>,
    },
    /// Structured plan a tool just published (US-009). Travels beside the
    /// textual result because it is addressed to the client, not to the model.
    Plan(PlanView),
    /// A hook ran around a tool call (US-017/US-019). Observational: the
    /// decision is already applied by the time this is emitted.
    Hook(crate::event::HookRunView),
}

type ToolEventEmitter =
    Arc<dyn Fn(ToolDispatchEvent) -> Result<(), ToolDispatchEvent> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolEventSink {
    emit: Option<ToolEventEmitter>,
}

impl ToolEventSink {
    pub fn new(tx: mpsc::UnboundedSender<ToolDispatchEvent>) -> Self {
        let emit: ToolEventEmitter = Arc::new(move |event| tx.send(event).map_err(|error| error.0));
        Self { emit: Some(emit) }
    }

    /// Builds a sink that forwards into another event transport. This keeps
    /// nested pipeline events on Code Mode's buffered bridge while preserving
    /// the same dispatch interface as a direct tool call.
    pub fn forwarding(forward: impl Fn(ToolDispatchEvent) + Send + Sync + 'static) -> Self {
        let emit: ToolEventEmitter = Arc::new(move |event| {
            forward(event);
            Ok(())
        });
        Self { emit: Some(emit) }
    }

    pub fn emit(&self, event: ToolDispatchEvent) {
        let _ = self.try_emit(event);
    }

    /// Emits when a receiver is attached, returning the event otherwise. Code
    /// Mode retains lifecycle events produced between `exec` and a later
    /// `wait` instead of losing them with the first dispatch channel.
    pub fn try_emit(&self, event: ToolDispatchEvent) -> Result<(), ToolDispatchEvent> {
        match &self.emit {
            Some(emit) => emit(event),
            None => Err(event),
        }
    }
}

#[async_trait::async_trait]
pub trait ToolDispatch: Send + Sync {
    /// Reseeds the permission taint from a resumed transcript. Implementations
    /// without taint can ignore this signal.
    fn seed_taint(&self, _recent_untrusted: bool) {}

    /// Nature of a call, for the client event. The dispatcher answers because it
    /// holds the actual tool; the loop only sees a name and a JSON value, which
    /// is not enough to tell an MCP tool from a built-in one that shares its
    /// name. Default `Other`: an implementation that says nothing loses nothing.
    fn call_kind(&self, _call: &ToolInvocation) -> crate::event::ToolCallKind {
        crate::event::ToolCallKind::Other
    }

    /// Runs a batch of calls and returns their results (order not guaranteed;
    /// each result is correlated by `id`).
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        events: ToolEventSink,
    ) -> Vec<ModelToolResult>;
}

#[cfg(test)]
mod tests {
    use agent_tokenizer::{HeuristicCounter, TokenCounter};

    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::function(
            name.to_string(),
            "test".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
    }

    #[test]
    fn identical_tool_sources_have_byte_stable_specs_and_fingerprint() {
        let specs = vec![spec("a"), spec("b")];
        let rebuilt = vec![spec("a"), spec("b")];
        assert_eq!(
            serde_json::to_vec(&specs).unwrap(),
            serde_json::to_vec(&rebuilt).unwrap()
        );
        assert_eq!(
            tool_plan_fingerprint(&specs, false),
            tool_plan_fingerprint(&rebuilt, false)
        );
        assert_ne!(
            tool_plan_fingerprint(&specs, false),
            tool_plan_fingerprint(&specs, true),
            "dispatch policy is part of the plan identity"
        );
    }

    #[test]
    fn token_truncation_keeps_utf8_and_shell_terminal_tail() {
        let counter = HeuristicCounter;
        let result = ModelToolResult::new(
            "call".into(),
            format!("{}{}", "é".repeat(2_000), "stdout-end"),
            true,
            true,
            Some(ToolErrorKind::Semantic),
        );
        let mut result = result;
        result.duration_ms = Some(42);
        result.execution = Some(ToolExecution {
            exit_code: Some(7),
            stderr_tail: Some("stderr-end".into()),
            ..ToolExecution::default()
        });
        let bounded = result.bound_feedback(&counter, 80, MAX_MODEL_TOOL_RESULT_BYTES);

        assert!(std::str::from_utf8(bounded.content.as_bytes()).is_ok());
        assert!(counter.count_text(&bounded.content) <= 80);
        assert!(
            bounded.content.contains("exit_code=7"),
            "{}",
            bounded.content
        );
        assert!(
            bounded.content.contains("stderr-end"),
            "{}",
            bounded.content
        );
        assert!(
            bounded.content.contains("duration_ms=42"),
            "{}",
            bounded.content
        );
        let truncation = bounded.truncation.expect("large result is truncated");
        assert_eq!(truncation.strategy, TruncationStrategy::Tail);
        assert!(truncation.original_bytes > truncation.kept_bytes);
    }

    #[test]
    fn terminal_metadata_survives_an_undersized_direct_budget() {
        let counter = HeuristicCounter;
        let mut result = ModelToolResult::new("call".into(), "x".repeat(10_000), true, true, None);
        result.duration_ms = Some(42);
        result.execution = Some(ToolExecution {
            exit_code: Some(9),
            signal: Some(15),
            timed_out: true,
            stderr_tail: Some("tail".repeat(1_000)),
            ..ToolExecution::default()
        });

        let bounded = result.bound_feedback(&counter, 1, 1);

        assert!(
            bounded.content.contains("exit_code=9"),
            "{}",
            bounded.content
        );
        assert!(bounded.content.contains("signal=15"), "{}", bounded.content);
        assert!(
            bounded.content.contains("timed_out=true"),
            "{}",
            bounded.content
        );
        assert!(
            bounded.content.contains("duration_ms=42"),
            "{}",
            bounded.content
        );
        assert!(bounded.content.len() <= MIN_MODEL_TOOL_RESULT_BYTES);
        assert!(counter.count_text(&bounded.content) <= MIN_MODEL_TOOL_RESULT_TOKENS);
    }

    #[test]
    fn structured_json_is_kept_whole_or_falls_back_to_bounded_text() {
        let counter = HeuristicCounter;
        let structured = serde_json::json!({"ok": true, "items": [1, 2, 3]});
        let mut result = ModelToolResult::new("call".into(), "fallback".into(), false, true, None);
        result.structured_content = Some(structured.clone());
        let kept = result.bound_feedback(&counter, 1_000, MAX_MODEL_TOOL_RESULT_BYTES);
        assert_eq!(kept.structured_content, Some(structured));
        let rendered = kept
            .content
            .strip_prefix("[structured_content]\n")
            .and_then(|value| value.split_once("\nfallback").map(|(json, _)| json))
            .expect("structured section");
        assert!(serde_json::from_str::<serde_json::Value>(rendered).is_ok());

        let mut huge =
            ModelToolResult::new("call".into(), "usable fallback".into(), false, true, None);
        let huge_structure = serde_json::json!({"blob": "x".repeat(10_000)});
        huge.structured_content = Some(huge_structure.clone());
        let bounded = huge.bound_feedback(&counter, 32, 128);
        assert_eq!(bounded.structured_content, Some(huge_structure));
        assert!(bounded.content.len() <= 128);
        assert!(std::str::from_utf8(bounded.content.as_bytes()).is_ok());
        assert!(!bounded.content.contains("\"blob\""));
    }
}
