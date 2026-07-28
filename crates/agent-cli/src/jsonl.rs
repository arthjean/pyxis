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
        f64::from(measured) / f64::from(estimated.max(1)),
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
    exit_code: i32,
    #[serde(flatten)]
    identity: &'a EventIdentity,
}

/// Line writer. In `Text` mode, everything is inert: not a byte is written,
/// which guarantees AC4 (default text output unchanged) by construction
/// rather than by re-reading.
pub struct EventWriter {
    format: OutputFormat,
    model_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
}

impl EventWriter {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
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
            Ok(json) => write_line(&json),
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
            Ok(json) => write_line(&json),
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
                exit_code,
                identity,
            },
        };
        match serde_json::to_string(&line) {
            Ok(json) => write_line(&json),
            Err(err) => eprintln!("[jsonl] summary not serializable: {err}"),
        }
    }
}

/// One line, flushed immediately: an orchestrator following the stream must not
/// wait for the end of the run to see the first event.
fn write_line(json: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "{json}").is_ok() {
        let _ = out.flush();
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
            is_error: false,
            error_kind: None,
            untrusted: true,
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
        let mut writer = EventWriter::new(OutputFormat::Text);
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
        let mut writer = EventWriter::new(OutputFormat::Text);
        assert!(!writer.is_json());
        // Nothing to observe on stdout: the absence of a write is structural
        // (early return on the format), not a property of this test.
        writer.identified_event(&AgentEvent::Text("x".into()), &EventIdentity::default());
        writer.run_summary("s.jsonl", &RunEnd::EndTurn, &EventIdentity::default());
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

        let exhausted =
            RunEnd::from_terminal(TurnState::Failed, Some("exhausted: MaxTurns(50)".into()));
        assert_eq!(exhausted, RunEnd::Exhausted("MaxTurns(50)".into()));
        assert_eq!(exhausted.parts().2, 1);

        let failed = RunEnd::from_terminal(TurnState::Failed, Some("provider down".into()));
        assert_eq!(failed, RunEnd::Error("provider down".into()));

        // A failed turn without a cause is still an error, named as such.
        assert!(matches!(
            RunEnd::from_terminal(TurnState::Failed, None),
            RunEnd::Error(_)
        ));
    }
}
