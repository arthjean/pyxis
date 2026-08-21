//! JSONL event output in headless mode (US-017).
//!
//! One JSON object per line on standard output, flushed immediately: an
//! automated caller (CI, orchestrator) reads the unfolding of a run without
//! parsing text meant for a human. The schema is documented in
//! `docs/EVENT_SCHEMA.md` and every line carries its version number, the only
//! way for a consumer to know whether it understands what it reads.
//!
//! This module only exists on the CLI side: `agent-core` stays I/O-free (invariant 1).
//! It makes no presentation decision either: the serialization is
//! that of `AgentEvent`, hence the contract, not a rendering.

use std::io::Write;

use agent_core::AgentEvent;
use serde::Serialize;

/// Version of the event schema. To be incremented as soon as an already emitted line
/// changes shape; adding a variant or an optional field does not change
/// how it is read and therefore does not increment it (same rule as `AgentEvent`).
pub const SCHEMA_VERSION: u32 = 1;

/// Output format of the headless mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Text of the final reply, as before US-017 (default).
    Text,
    /// One JSON event per line.
    Json,
}

pub fn output_format_from_arg(arg: &str) -> Option<OutputFormat> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "text" => Some(OutputFormat::Text),
        "json" | "jsonl" | "stream-json" => Some(OutputFormat::Json),
        _ => None,
    }
}

/// Calibration line of the usage probe (US-002, `PYXIS_DEBUG_USAGE`). The core
/// carries the backend measure and the local estimate in `ModelTurn`; deciding
/// where to write them belongs to the binary (invariant 1). `None` as soon as
/// one of the two sides is missing: a ratio against an absent measure would
/// mean nothing.
pub fn usage_probe_line(view: &agent_core::ModelTurnView) -> Option<String> {
    let (measured, estimated) = (view.context_tokens?, view.estimated_context_tokens?);
    Some(format!(
        "[usage] turn={} backend input={measured} | local estimate≈{estimated} \
         (ratio real/estimated={:.3})",
        view.index,
        measured as f64 / f64::from(estimated.max(1)),
    ))
}

/// Durable identity of a line (US-018 AC2). Added NEXT TO the existing fields,
/// never in place of one: a consumer that ignores them reads exactly what it
/// read before (AC3). Absent when the run has no runtime identity to report,
/// which keeps them out of the way rather than emitting nulls to handle.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EventIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

/// Event line. `event` is flattened: the shape of `AgentEvent`
/// (`{"type": ..., "data": ...}`) stays visible as is, augmented with the `schema`.
#[derive(Serialize)]
struct EventLine<'a> {
    schema: u32,
    #[serde(flatten)]
    event: &'a AgentEvent,
    #[serde(flatten)]
    identity: &'a EventIdentity,
}

#[derive(Serialize)]
struct StoreFailedLine<'a> {
    schema: u32,
    r#type: &'static str,
    data: StoreFailedData<'a>,
}

#[derive(Serialize)]
struct StoreFailedData<'a> {
    operation: agent_runtime::StoreOperation,
    detail: &'a str,
    #[serde(flatten)]
    identity: &'a EventIdentity,
}

/// How a headless run ended, as the runtime reports it.
///
/// `Interrupted` is the variant `HeadlessEnd` never had: before EP-005 nothing
/// could interrupt a `-p` run, so the case could not be observed. It is now
/// reachable through Ctrl+C and reported as what it is rather than as a
/// successful end of turn (US-018 AC5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    EndTurn,
    Interrupted(Option<String>),
    Exhausted(String),
    Error(String),
}

impl RunEnd {
    /// Terminal state of the turn, as the durable log names it.
    pub fn from_terminal(state: agent_runtime::TurnState, cause: Option<String>) -> Self {
        match state {
            agent_runtime::TurnState::Completed => Self::EndTurn,
            agent_runtime::TurnState::Interrupted => Self::Interrupted(cause),
            // `failed` covers both a real error and a guardrail stop; the cause
            // the runtime recorded is what tells them apart.
            _ => match cause {
                Some(cause) if cause.starts_with("exhausted: ") => {
                    Self::Exhausted(cause["exhausted: ".len()..].to_string())
                }
                Some(cause) => Self::Error(cause),
                None => Self::Error("turn failed without a recorded cause".into()),
            },
        }
    }

    /// Label, detail and exit code of a run end. The exit code is the ONLY
    /// thing a pipeline that ignores the stream can read, so it is decided here,
    /// next to the label, and never twice.
    fn parts(&self) -> (&'static str, Option<String>, i32) {
        match self {
            Self::EndTurn => ("end_turn", None, 0),
            Self::Interrupted(cause) => ("interrupted", cause.clone(), 1),
            Self::Exhausted(reason) => ("exhausted", Some(reason.clone()), 1),
            Self::Error(err) => ("error", Some(err.clone()), 1),
        }
    }

    /// The failure this end carries, read through the SAME classifier the TUI
    /// and the app-server use (US-019 AC1). `end` says which terminal was
    /// reached; the category says which diagnostic follows, and the two are not
    /// the same question: `error` covers a provider outage and a revoked
    /// credential alike.
    pub fn failure(&self) -> Option<agent_runtime::TurnFailure> {
        match self {
            Self::EndTurn => None,
            Self::Interrupted(cause) => {
                cause.as_deref().map(agent_runtime::TurnFailure::from_cause)
            }
            // `from_terminal` stripped the prefix the log carried; the cause is
            // rebuilt so one classifier keeps deciding, not two.
            Self::Exhausted(reason) => Some(agent_runtime::TurnFailure::from_cause(&format!(
                "exhausted: {reason}"
            ))),
            Self::Error(err) => Some(agent_runtime::TurnFailure::from_cause(err)),
        }
    }
}

/// End-of-run summary (AC3). This is NOT an `AgentEvent`: the session identifier
/// and the exit code are process facts, which the core has
/// no reason to know.
#[derive(Serialize)]
struct SummaryLine<'a> {
    schema: u32,
    r#type: &'static str,
    data: RunSummary<'a>,
}

#[derive(Serialize)]
struct RunSummary<'a> {
    session_id: &'a str,
    /// Number of model round-trips observed (`model_turn`).
    model_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    /// End cause: `end_turn`, `interrupted`, `exhausted` or `error`.
    end: &'static str,
    /// Detail of the cause when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    end_detail: Option<String>,
    /// Category the shared classifier read out of the cause, in the same
    /// vocabulary the app-server publishes (US-019 AC1). Absent on a clean end.
    #[serde(skip_serializing_if = "Option::is_none")]
    cause_category: Option<&'static str>,
    /// The next diagnostic step for that category, so a log read after the fact
    /// says what to do and not only what broke.
    #[serde(skip_serializing_if = "Option::is_none")]
    cause_guidance: Option<&'static str>,
    exit_code: i32,
    #[serde(flatten)]
    identity: &'a EventIdentity,
}

/// Line writer. In `Text` mode, everything is inert: not a byte is written,
/// which guarantees AC4 (default text output unchanged) by construction
/// rather than by re-reading.
///
/// The sink is handed in rather than decided here (US-120). What a run really
/// renders is the byte sequence this writer produces, and a test cannot read it
/// while the destination is `stdout()` locked from inside a free function: it
/// would have to parse the harness's own output. A trait object rather than a
/// type parameter, because the writer is built once per run and never in a hot
/// loop, and because a generic would propagate into `HeadlessRun` and from there
/// into every caller of `headless::run`.
pub struct EventWriter {
    format: OutputFormat,
    sink: Box<dyn Write + Send>,
    model_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
}

impl EventWriter {
    pub fn new(format: OutputFormat, sink: Box<dyn Write + Send>) -> Self {
        Self {
            format,
            sink,
            model_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn is_json(&self) -> bool {
        self.format == OutputFormat::Json
    }

    /// Writes an event, with the durable identity the runtime correlated it to,
    /// and updates the summary accounting. Called for EVERY event, including in
    /// text mode, so that the counting does not depend on the chosen format.
    pub fn identified_event(&mut self, event: &AgentEvent, identity: &EventIdentity) {
        if let AgentEvent::ModelTurn(view) = event {
            self.model_turns = self.model_turns.max(view.index);
            self.input_tokens = view.input_tokens;
            self.output_tokens = view.output_tokens;
            // Calibration probe: stderr, so the text output on stdout stays
            // identical byte for byte, and only when the probe is enabled (the
            // core then fills the estimate).
            if let Some(line) = usage_probe_line(view) {
                eprintln!("{line}");
            }
        }
        if self.format != OutputFormat::Json {
            return;
        }
        let line = EventLine {
            schema: SCHEMA_VERSION,
            event,
            identity,
        };
        // A non-serializable event would be a contract bug, not a
        // runtime condition: we report it on stderr without cutting the stream.
        match serde_json::to_string(&line) {
            Ok(json) => self.write_line(&json),
            Err(err) => eprintln!("[jsonl] event not serializable: {err}"),
        }
    }

    /// Live-only machine event for a writer that can no longer persist its own
    /// health transition.
    pub fn thread_store_failed(
        &mut self,
        operation: agent_runtime::StoreOperation,
        detail: &str,
        identity: &EventIdentity,
    ) {
        if self.format != OutputFormat::Json {
            eprintln!("[runtime] thread store failed during {operation}: {detail}");
            return;
        }
        let line = StoreFailedLine {
            schema: SCHEMA_VERSION,
            r#type: "thread_store_failed",
            data: StoreFailedData {
                operation,
                detail,
                identity,
            },
        };
        match serde_json::to_string(&line) {
            Ok(json) => self.write_line(&json),
            Err(err) => eprintln!("[jsonl] store failure not serializable: {err}"),
        }
    }

    /// Last line of the run (AC3, AC6). `exit_code` distinguishes a success from a
    /// failure for a caller that would only read the return code.
    pub fn run_summary(&mut self, session_id: &str, ended: &RunEnd, identity: &EventIdentity) {
        if self.format != OutputFormat::Json {
            return;
        }
        let (end, end_detail, exit_code) = ended.parts();
        let failure = ended.failure();
        let line = SummaryLine {
            schema: SCHEMA_VERSION,
            r#type: "run_summary",
            data: RunSummary {
                session_id,
                model_turns: self.model_turns,
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                end,
                end_detail,
                cause_category: failure.as_ref().map(|f| f.category.label()),
                cause_guidance: failure.as_ref().map(|f| f.category.guidance()),
                exit_code,
                identity,
            },
        };
        match serde_json::to_string(&line) {
            Ok(json) => self.write_line(&json),
            Err(err) => eprintln!("[jsonl] summary not serializable: {err}"),
        }
    }

    /// One line, flushed immediately: an orchestrator following the stream must
    /// not wait for the end of the run to see the first event.
    ///
    /// A sink that refuses the write is swallowed, as it was when the sink was
    /// `stdout()`: a consumer that closed the pipe ends the stream, not the
    /// turn, whose terminal state is durable elsewhere.
    fn write_line(&mut self, json: &str) {
        if writeln!(self.sink, "{json}").is_ok() {
            let _ = self.sink.flush();
        }
    }
}

/// Sink that keeps every byte written to it, shared with the test that reads
/// them back (US-120). The contract is the byte sequence, so the harness asserts
/// on bytes and never on a `serde_json::Value` rebuilt from the same struct the
/// writer serialized.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct CapturedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl CapturedSink {
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().map(|held| held.clone()).unwrap_or_default()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

#[cfg(test)]
impl Write for CapturedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut held) = self.0.lock() {
            held.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sink that refuses every write, the way a closed pipe does.
#[cfg(test)]
pub struct FailingSink;

#[cfg(test)]
impl Write for FailingSink {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "sink closed",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "sink closed",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{ModelTurnView, ToolResultView};

    fn line_of(event: &AgentEvent) -> serde_json::Value {
        let line = EventLine {
            schema: SCHEMA_VERSION,
            event,
            identity: &EventIdentity::default(),
        };
        serde_json::to_value(&line).unwrap()
    }

    #[test]
    fn output_format_parses_aliases_and_rejects_garbage() {
        assert_eq!(output_format_from_arg("text"), Some(OutputFormat::Text));
        assert_eq!(output_format_from_arg(" JSON "), Some(OutputFormat::Json));
        assert_eq!(
            output_format_from_arg("stream-json"),
            Some(OutputFormat::Json)
        );
        assert_eq!(output_format_from_arg("yaml"), None);
    }

    /// AC2: every line carries the schema version, and the shape of `AgentEvent`
    /// stays readable as is next to it.
    #[test]
    fn every_line_carries_the_schema_version_and_the_event_shape() {
        let value = line_of(&AgentEvent::Text("bonjour".into()));
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["type"], "text");
        assert_eq!(value["data"], "bonjour");

        // Unit variant: always an object, never a bare string.
        let value = line_of(&AgentEvent::EndTurn);
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["type"], "end_turn");
    }

    #[test]
    fn store_failure_is_a_machine_event_and_an_error_summary() {
        let identity = EventIdentity {
            thread_id: Some("thr_test".into()),
            turn_id: Some("trn_test".into()),
            event_id: Some("evt_test".into()),
        };
        let value = serde_json::to_value(StoreFailedLine {
            schema: SCHEMA_VERSION,
            r#type: "thread_store_failed",
            data: StoreFailedData {
                operation: agent_runtime::StoreOperation::Append,
                detail: "writer poisoned",
                identity: &identity,
            },
        })
        .unwrap();
        assert_eq!(value["type"], "thread_store_failed");
        assert_eq!(value["data"]["operation"], "append");
        assert_eq!(value["data"]["detail"], "writer poisoned");
        assert_eq!(value["data"]["thread_id"], "thr_test");

        let (end, detail, exit_code) = RunEnd::Error("store failed".into()).parts();
        assert_eq!(end, "error");
        assert_eq!(detail.as_deref(), Some("store failed"));
        assert_ne!(exit_code, 0);
    }

    /// US-009 AC3/AC5: the plan is emitted on the JSONL stream exactly like any
    /// other event, under its own `type`. A consumer that filters on the types
    /// it knows therefore ignores it without a single behavior change, and the
    /// schema version does not move: adding a variant changes nothing in how an
    /// already emitted line reads.
    #[test]
    fn the_plan_is_one_more_typed_line_on_the_stream() {
        let value = line_of(&AgentEvent::Plan(agent_core::PlanView {
            explanation: Some("cadrage".into()),
            steps: vec![
                agent_core::PlanStep {
                    step: "lire".into(),
                    status: agent_core::PlanStatus::Completed,
                },
                agent_core::PlanStep {
                    step: "écrire".into(),
                    status: agent_core::PlanStatus::InProgress,
                },
            ],
        }));
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["type"], "plan");
        assert_eq!(value["data"]["explanation"], "cadrage");
        assert_eq!(value["data"]["steps"][1]["status"], "in_progress");

        // An absent explanation is absent from the line, not a null to handle.
        let value = line_of(&AgentEvent::Plan(agent_core::PlanView::default()));
        assert!(value["data"].get("explanation").is_none());
    }

    /// AC5: non-textual content and control characters -> the line stays a
    /// valid, parsable JSON, without a raw byte nor an internal newline.
    #[test]
    fn control_characters_stay_inside_one_valid_json_line() {
        let hostile = "ligne1\nligne2\u{0}\u{1b}[31mrouge\u{7}\r\t\"quote\"\\";
        let event = AgentEvent::ToolResult(ToolResultView {
            id: "call_1".to_string(),
            content: hostile.to_string(),
            status: None,
            structured_content: None,
            is_error: false,
            error_kind: None,
            untrusted: true,
            duration_ms: None,
            truncation: None,
            execution: None,
        });

        let json = serde_json::to_string(&EventLine {
            schema: SCHEMA_VERSION,
            event: &event,
            identity: &EventIdentity::default(),
        })
        .unwrap();

        assert!(
            !json.contains('\n'),
            "un saut de ligne brut casserait le découpage JSONL: {json}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["data"]["content"], hostile);
    }

    /// AC3: the summary counts the turns and takes the tokens of the last
    /// `model_turn` observed, without adding them twice (they are cumulated).
    #[test]
    fn summary_tracks_the_last_cumulative_counters() {
        let mut writer = EventWriter::new(OutputFormat::Text, Box::new(CapturedSink::default()));
        for index in 1..=3 {
            writer.identified_event(
                &AgentEvent::ModelTurn(ModelTurnView {
                    index,
                    input_tokens: u64::from(index) * 100,
                    output_tokens: u64::from(index) * 10,
                    ..ModelTurnView::default()
                }),
                &EventIdentity::default(),
            );
        }

        assert_eq!(writer.model_turns, 3);
        assert_eq!(writer.input_tokens, 300);
        assert_eq!(writer.output_tokens, 30);
    }

    /// US-002: the probe only says something when it can compare the two sides.
    #[test]
    fn usage_probe_needs_both_sides() {
        let base = ModelTurnView {
            index: 4,
            input_tokens: 10,
            output_tokens: 2,
            ..ModelTurnView::default()
        };
        assert!(
            usage_probe_line(&base).is_none(),
            "sonde inactive: aucune ligne"
        );
        assert!(
            usage_probe_line(&ModelTurnView {
                context_tokens: Some(1_000),
                ..base
            })
            .is_none(),
            "mesure sans estimation: rien à comparer"
        );
        let line = usage_probe_line(&ModelTurnView {
            context_tokens: Some(1_000),
            estimated_context_tokens: Some(500),
            ..base
        })
        .expect("sonde active");
        assert!(line.contains("turn=4"), "{line}");
        assert!(line.contains("ratio real/estimated=2.000"), "{line}");
    }

    #[test]
    fn text_format_writes_nothing() {
        let sink = CapturedSink::default();
        let mut writer = EventWriter::new(OutputFormat::Text, Box::new(sink.clone()));
        assert!(!writer.is_json());
        writer.identified_event(&AgentEvent::Text("x".into()), &EventIdentity::default());
        writer.run_summary("s.jsonl", &RunEnd::EndTurn, &EventIdentity::default());
        // The absence of a write is structural (early return on the format);
        // now that the sink is given, the test reads it instead of asserting it.
        assert!(sink.bytes().is_empty(), "text mode must not write a byte");
    }

    /// US-120 AC3/AC5: the harness reads the BYTES the writer renders, not a
    /// `Value` rebuilt from the same struct. `\n` separates the records, so its
    /// presence at the end of every line and the absence of `\r` are the two
    /// halves of the JSONL contract that a `to_value` round trip cannot see.
    #[test]
    fn a_written_line_ends_with_a_bare_newline_and_carries_no_carriage_return() {
        let sink = CapturedSink::default();
        let mut writer = EventWriter::new(OutputFormat::Json, Box::new(sink.clone()));
        writer.identified_event(
            &AgentEvent::Text("bonjour".into()),
            &EventIdentity::default(),
        );
        writer.identified_event(&AgentEvent::EndTurn, &EventIdentity::default());
        writer.run_summary("s.jsonl", &RunEnd::EndTurn, &EventIdentity::default());

        let bytes = sink.bytes();
        assert_eq!(bytes.last(), Some(&b'\n'), "the last byte is the separator");
        assert!(
            !bytes.contains(&b'\r'),
            "a CRLF would break the record split: {}",
            sink.text()
        );
        let text = sink.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event plus the summary");
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line).expect("each line parses on its own");
        }
        assert!(
            lines[0].starts_with("{\"schema\":1,\"type\":\"text\""),
            "{}",
            lines[0]
        );
        assert!(
            lines[2].contains("\"type\":\"run_summary\""),
            "{}",
            lines[2]
        );
    }

    /// US-120 AC4: a sink that refuses every write is what a closed pipe is. The
    /// error was ignored when the sink was `stdout()` and stays ignored: the
    /// terminal state of the turn is durable in the thread log, not in the
    /// stream, so losing the stream must not end the run.
    #[test]
    fn a_sink_that_always_fails_does_not_stop_the_writer() {
        let mut writer = EventWriter::new(OutputFormat::Json, Box::new(FailingSink));
        writer.identified_event(
            &AgentEvent::Text("bonjour".into()),
            &EventIdentity::default(),
        );
        writer.thread_store_failed(
            agent_runtime::StoreOperation::Append,
            "writer poisoned",
            &EventIdentity::default(),
        );
        writer.run_summary("s.jsonl", &RunEnd::EndTurn, &EventIdentity::default());
        // Reached: not a single write panicked or short-circuited the caller.
        assert!(writer.is_json());
    }

    /// US-018 AC2/AC3: the identity is added NEXT TO the existing fields. A line
    /// read by a client that ignores them is byte-identical to what it was.
    #[test]
    fn the_runtime_identity_is_additive() {
        let event = AgentEvent::Text("bonjour".into());
        let before = line_of(&event);

        let identity = EventIdentity {
            thread_id: Some("th_1".into()),
            turn_id: Some("tu_2".into()),
            event_id: Some("ev_3".into()),
        };
        let after = serde_json::to_value(EventLine {
            schema: SCHEMA_VERSION,
            event: &event,
            identity: &identity,
        })
        .unwrap();

        assert_eq!(after["schema"], before["schema"]);
        assert_eq!(after["type"], before["type"]);
        assert_eq!(after["data"], before["data"]);
        assert_eq!(after["thread_id"], "th_1");
        assert_eq!(after["turn_id"], "tu_2");
        assert_eq!(after["event_id"], "ev_3");
    }

    /// US-018 AC5: an interruption is reported as one, with a non-zero exit
    /// code, instead of borrowing the shape of a clean end of turn.
    #[test]
    fn a_terminal_state_maps_to_its_own_end_and_exit_code() {
        use agent_runtime::TurnState;

        assert_eq!(
            RunEnd::from_terminal(TurnState::Completed, None),
            RunEnd::EndTurn
        );
        assert_eq!(RunEnd::EndTurn.parts().2, 0);

        let interrupted = RunEnd::from_terminal(TurnState::Interrupted, Some("shutdown".into()));
        assert_eq!(interrupted.parts().0, "interrupted");
        assert_eq!(interrupted.parts().2, 1);

        let exhausted = RunEnd::from_terminal(
            TurnState::Failed,
            Some("exhausted: ToolLoop { count: 4 }".into()),
        );
        assert_eq!(exhausted, RunEnd::Exhausted("ToolLoop { count: 4 }".into()));
        assert_eq!(exhausted.parts().2, 1);

        let failed = RunEnd::from_terminal(TurnState::Failed, Some("provider down".into()));
        assert_eq!(failed, RunEnd::Error("provider down".into()));

        // A failed turn without a cause is still an error, named as such.
        assert!(matches!(
            RunEnd::from_terminal(TurnState::Failed, None),
            RunEnd::Error(_)
        ));
    }

    /// US-019 AC1: the summary line names the CATEGORY and the next step, in the
    /// vocabulary the TUI prints and the app-server publishes. `end` says which
    /// terminal was reached, `cause_category` says which diagnostic follows, and
    /// a pipeline needs both.
    #[test]
    fn the_summary_names_the_category_and_the_next_step() {
        use agent_runtime::{FailureCategory, TurnState};

        let failed = RunEnd::from_terminal(TurnState::Failed, Some("auth: Expired".into()));
        let failure = failed.failure().expect("a failed run carries a failure");
        assert_eq!(failure.category, FailureCategory::Auth);
        assert_eq!(failure.category.label(), "auth");
        assert!(failure.category.guidance().contains("/login"));

        // A guardrail stop is a failure of its own kind, not a provider one:
        // `from_terminal` strips the prefix and `failure()` rebuilds it, so one
        // classifier keeps deciding.
        let exhausted = RunEnd::from_terminal(
            TurnState::Failed,
            Some("exhausted: ToolLoop { count: 4 }".into()),
        );
        assert_eq!(
            exhausted.failure().map(|f| f.category),
            Some(FailureCategory::Guardrail)
        );

        // A clean end has nothing to categorize, so the fields stay absent.
        assert!(RunEnd::EndTurn.failure().is_none());
        assert!(RunEnd::Interrupted(None).failure().is_none());
    }

    /// The two fields are ADDITIVE: a consumer that ignores them reads the same
    /// line it read before, and a clean run gets neither.
    #[test]
    fn the_category_fields_are_additive_and_absent_on_a_clean_run() {
        let clean = summary_value(&RunEnd::EndTurn);
        assert_eq!(clean["data"]["end"], "end_turn");
        assert!(clean["data"].get("cause_category").is_none(), "{clean}");
        assert!(clean["data"].get("cause_guidance").is_none(), "{clean}");

        let failed = summary_value(&RunEnd::Error("provider: 503".into()));
        assert_eq!(failed["data"]["end"], "error");
        assert_eq!(failed["data"]["end_detail"], "provider: 503");
        assert_eq!(failed["data"]["cause_category"], "provider");
        assert_eq!(failed["data"]["exit_code"], 1);
    }

    /// The summary line as it is serialized, without going through stdout.
    fn summary_value(ended: &RunEnd) -> serde_json::Value {
        let (end, end_detail, exit_code) = ended.parts();
        let failure = ended.failure();
        let identity = EventIdentity::default();
        serde_json::to_value(SummaryLine {
            schema: SCHEMA_VERSION,
            r#type: "run_summary",
            data: RunSummary {
                session_id: "s.jsonl",
                model_turns: 0,
                input_tokens: 0,
                output_tokens: 0,
                end,
                end_detail,
                cause_category: failure.as_ref().map(|f| f.category.label()),
                cause_guidance: failure.as_ref().map(|f| f.category.guidance()),
                exit_code,
                identity: &identity,
            },
        })
        .expect("the summary serializes")
    }

    /// US-075 AC3: a spilled result serialized to JSONL carries the locator
    /// exactly as the store produced it, workspace-relative, and the line holds
    /// no absolute path. Built from the real policy rather than a hand-written
    /// struct: the string under test is the one a run would actually emit.
    #[test]
    fn a_spilled_result_serializes_a_relative_locator_and_no_absolute_path() {
        let workspace = std::env::temp_dir().join(format!(
            "pyxis-jsonl-spill-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        let store = agent_tools::spill::SpillStore::create(&workspace, "thread-jsonl")
            .expect("a spill root");

        let content = "une ligne de sortie\n".repeat(60_000);
        let spilled =
            agent_tools::spill_policy::replace_with_spill(&store, "grep", "call_1", &content)
                .expect("an oversized output is replaced");

        let value = line_of(&AgentEvent::ToolResult(ToolResultView {
            id: "call_1".to_string(),
            content: spilled.content,
            status: None,
            structured_content: None,
            is_error: false,
            error_kind: None,
            untrusted: true,
            duration_ms: None,
            truncation: Some(spilled.truncation),
            execution: None,
        }));

        let hint = value["data"]["truncation"]["continuation_hint"]
            .as_str()
            .expect("the locator rides `truncation`");
        assert!(
            hint.starts_with(".pyxis/spill/"),
            "le localisateur doit rester relatif au workspace: {hint}"
        );

        let line = serde_json::to_string(&value).unwrap();
        for absolute in [
            workspace.to_str().expect("a utf8 temp path"),
            store.root().to_str().expect("a utf8 root"),
        ] {
            assert!(
                !line.contains(absolute),
                "aucun chemin absolu ne doit fuir dans la ligne: {absolute}"
            );
        }
        // The notice the model reads quotes the very same relative locator.
        assert!(
            value["data"]["content"]
                .as_str()
                .unwrap_or_default()
                .contains(hint),
            "la note doit citer le localisateur sérialisé"
        );
        assert_eq!(
            value["data"]["truncation"]["original_bytes"]
                .as_u64()
                .unwrap_or_default(),
            content.len() as u64
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// US-075 AC4: the clients carry the locator, they never parse it. A split,
    /// a join or a re-rooting in the TUI or in the app-server would tie the two
    /// crates to the storage layout that EP-022 owns, so the scan denies any
    /// mention of the field outside the core that produces it.
    #[test]
    fn no_client_crate_takes_the_continuation_hint_apart() {
        fn rust_sources(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rust_sources(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && let Ok(source) = std::fs::read_to_string(&path)
                {
                    out.push((path, source));
                }
            }
        }

        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [
            manifest.join("..").join("agent-tui").join("src"),
            manifest.join("..").join("agent-app-server").join("src"),
        ];
        let mut sources = Vec::new();
        for root in &roots {
            assert!(root.is_dir(), "source root introuvable: {}", root.display());
            rust_sources(root, &mut sources);
        }
        assert!(
            sources.len() >= 10,
            "seulement {} fichiers scannés: le scan n'atteint pas les sources",
            sources.len()
        );

        let offenders: Vec<String> = sources
            .iter()
            .filter(|(_, source)| source.contains("continuation_hint"))
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert!(
            offenders.is_empty(),
            "un client ne doit ni découper ni reconstruire le localisateur:\n{}",
            offenders.join("\n")
        );
    }
}
