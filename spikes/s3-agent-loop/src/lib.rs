//! US-003: minimal loop stream -> Bash tool -> reinjection -> loop back.
//!
//! Validates the state machine with typed transitions in its **most reduced** form
//! (see docs/ROADMAP.md Phase 0): exhaustive `enum Transition`, dispatch of a single
//! tool (`bash`) under `tokio::time::timeout`, reinjection of the result, loop back
//! until `end_turn`. The Compact/Recover transitions of the full architecture
//! (US-006/US-008) are out of scope here, by roadmap decision.
//!
//! `Provider` is an injectable trait (see the "injectable deps" invariant): the
//! loop is testable without a real API through `ScriptedProvider`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use futures_util::stream::BoxStream;
use std::collections::HashMap;
use std::time::Duration;

pub use spike_canon::{AdapterError, StopReason, StreamEvent};

// ───────────────────────────── Injectable provider ──────────────────────────

/// Source of `StreamEvent` (real or scripted). Object-safe: the loop takes
/// a `&dyn Provider`.
pub trait Provider: Send + Sync {
    fn stream(
        &self,
        messages: Vec<serde_json::Value>,
    ) -> BoxStream<'static, Result<StreamEvent, AdapterError>>;
}

/// Scripted provider for the tests: returns a fixed list of events per turn.
pub struct ScriptedProvider {
    turns: std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns.into()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        _messages: Vec<serde_json::Value>,
    ) -> BoxStream<'static, Result<StreamEvent, AdapterError>> {
        use futures_util::StreamExt;
        let turn = self
            .turns
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
            .unwrap_or_default();
        async_stream::stream! {
            for e in turn {
                yield Ok(e);
            }
        }
        .boxed()
    }
}

/// Live OpenAI-compatible provider (Ollama / OpenAI), through `spike_canon`.
pub struct LiveProvider {
    pub base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub tools: Option<serde_json::Value>,
}

impl Provider for LiveProvider {
    fn stream(
        &self,
        messages: Vec<serde_json::Value>,
    ) -> BoxStream<'static, Result<StreamEvent, AdapterError>> {
        use futures_util::StreamExt;
        let base = self.base.clone();
        let key = self.api_key.clone();
        let body = spike_canon::build_body(
            &self.model,
            serde_json::Value::Array(messages),
            self.tools.clone(),
        );
        async_stream::stream! {
            match spike_canon::stream_chat(&base, key.as_deref(), body).await {
                Ok(mut s) => {
                    while let Some(ev) = s.next().await {
                        yield ev;
                    }
                }
                Err(e) => yield Err(e),
            }
        }
        .boxed()
    }
}

// ───────────────────────────── Turn accumulator ─────────────────────────

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args_json: String,
}

struct PartialCall {
    name: String,
    args: String,
}

/// Accumulates the `StreamEvent` of a turn into a decision state.
#[derive(Default)]
pub struct Accumulator {
    pub text: String,
    pub reasoning: String,
    pub stop: Option<StopReason>,
    open: HashMap<String, PartialCall>,
    order: Vec<String>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::TextDelta { text } => self.text.push_str(&text),
            StreamEvent::ReasoningDelta { text } => self.reasoning.push_str(&text),
            StreamEvent::ToolCallStart { id, name } => {
                self.open.insert(
                    id.clone(),
                    PartialCall {
                        name,
                        args: String::new(),
                    },
                );
                self.order.push(id);
            }
            StreamEvent::ToolCallDelta { id, args_json } => {
                if let Some(p) = self.open.get_mut(&id) {
                    p.args.push_str(&args_json);
                } else {
                    self.open.insert(
                        id.clone(),
                        PartialCall {
                            name: String::new(),
                            args: args_json,
                        },
                    );
                    self.order.push(id);
                }
            }
            StreamEvent::ToolCallEnd { .. } | StreamEvent::Usage { .. } => {}
            StreamEvent::Done { stop } => self.stop = Some(stop),
        }
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.order
            .iter()
            .filter_map(|id| {
                self.open.get(id).map(|p| ToolCall {
                    id: id.clone(),
                    name: p.name.clone(),
                    args_json: p.args.clone(),
                })
            })
            .collect()
    }
}

// ──────────────────────────── Typed state machine ───────────────────────────

/// Exhaustive transition (reduced Phase 0 form). The driver `match` forces
/// every case to be handled -> control flow checked at compile time.
#[derive(Debug)]
pub enum Transition {
    /// The model finished without tool_use -> hand back control.
    EndTurn,
    /// The model asks for tools -> run them then loop back.
    RunTools(Vec<ToolCall>),
    /// Turn cap / max_tokens.
    Exhausted(String),
    /// Fatal error -> propagate.
    Fail(String),
}

/// Pure, no I/O -> unit-testable (the crux of headless testability).
pub fn decide_transition(acc: &Accumulator) -> Transition {
    let calls = acc.tool_calls();
    if !calls.is_empty() && matches!(acc.stop, Some(StopReason::ToolUse)) {
        return Transition::RunTools(calls);
    }
    match acc.stop {
        Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => Transition::EndTurn,
        Some(StopReason::MaxTokens) => Transition::Exhausted("max_tokens".to_string()),
        Some(StopReason::Refusal) => Transition::Fail("refusal".to_string()),
        // ToolUse announced but no call assembled -> fail-closed to EndTurn.
        Some(StopReason::ToolUse) => Transition::EndTurn,
    }
}

// ─────────────────────────────── Bash tool ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub name: String,
    pub args: String,
    pub output: String,
    /// Every tool output is untrusted by default (taint = US-013).
    pub untrusted: bool,
    pub timed_out: bool,
}

/// Runs `bash -c <cmd>` under a timeout. A tool that hangs does not freeze the loop:
/// the timeout takes control back (`kill_on_drop` kills the orphan process).
pub async fn exec_bash(id_name: &str, args_json: &str, timeout: Duration) -> ToolInvocation {
    let cmd = serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| {
            v.get("cmd")
                .or_else(|| v.get("command"))
                .and_then(|s| s.as_str().map(str::to_string))
        })
        .unwrap_or_default();

    if cmd.is_empty() {
        return ToolInvocation {
            name: id_name.to_string(),
            args: args_json.to_string(),
            output: "erreur: argument `cmd` manquant ou args non-JSON".to_string(),
            untrusted: true,
            timed_out: false,
        };
    }

    let fut = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .kill_on_drop(true)
        .output();

    let (output, timed_out) = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                s.push_str(&err);
            }
            (s.trim().to_string(), false)
        }
        Ok(Err(e)) => (format!("erreur exec: {e}"), false),
        Err(_) => (
            "[timeout] outil interrompu — la boucle reprend la main".to_string(),
            true,
        ),
    };

    ToolInvocation {
        name: id_name.to_string(),
        args: cmd,
        output,
        untrusted: true,
        timed_out,
    }
}

// ──────────────────────────────── Driver ────────────────────────────────────

#[derive(Debug)]
pub enum EndState {
    EndTurn,
    Exhausted(String),
    Fail(String),
}

#[derive(Debug)]
pub struct RunOutcome {
    pub final_text: String,
    pub turns: usize,
    pub invocations: Vec<ToolInvocation>,
    pub ended: EndState,
}

/// The full loop: stream -> decision -> (tool -> reinjection -> loop back) | end.
pub async fn run_agent(
    provider: &dyn Provider,
    system: Option<&str>,
    user: &str,
    max_turns: usize,
    tool_timeout: Duration,
) -> RunOutcome {
    use futures_util::StreamExt;

    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = system {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": user }));

    let mut invocations = Vec::new();
    let mut turns = 0usize;

    loop {
        if turns >= max_turns {
            return RunOutcome {
                final_text: String::new(),
                turns,
                invocations,
                ended: EndState::Exhausted(format!("max_turns={max_turns}")),
            };
        }
        turns += 1;

        // transcript-before-response would go here (US-006/US-009): out of spike scope.
        let mut acc = Accumulator::new();
        let mut stream = provider.stream(messages.clone());
        let mut stream_err = None;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(se) => acc.push(se),
                Err(e) => {
                    stream_err = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = stream_err {
            return RunOutcome {
                final_text: acc.text,
                turns,
                invocations,
                ended: EndState::Fail(e),
            };
        }

        match decide_transition(&acc) {
            Transition::EndTurn => {
                return RunOutcome {
                    final_text: acc.text,
                    turns,
                    invocations,
                    ended: EndState::EndTurn,
                };
            }
            Transition::Exhausted(why) => {
                return RunOutcome {
                    final_text: acc.text,
                    turns,
                    invocations,
                    ended: EndState::Exhausted(why),
                };
            }
            Transition::Fail(why) => {
                return RunOutcome {
                    final_text: acc.text,
                    turns,
                    invocations,
                    ended: EndState::Fail(why),
                };
            }
            Transition::RunTools(calls) => {
                // assistant message (with tool_calls) added to the transcript
                let tool_calls_json: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.args_json },
                        })
                    })
                    .collect();
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": acc.text,
                    "tool_calls": tool_calls_json,
                }));

                // execution + reinjection of each result
                for call in calls {
                    let inv = exec_bash(&call.name, &call.args_json, tool_timeout).await;
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "content": inv.output,
                    }));
                    invocations.push(inv);
                }
                // loop back: the model sees the results
            }
        }
    }
}

// ─────────────────────────────────── Tests ──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_turn(id: &str, cmd: &str) -> Vec<StreamEvent> {
        let args = serde_json::json!({ "cmd": cmd }).to_string();
        vec![
            StreamEvent::ToolCallStart {
                id: id.into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                id: id.into(),
                args_json: args,
            },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]
    }

    fn text_turn(t: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: t.into() },
            StreamEvent::Done {
                stop: StopReason::EndTurn,
            },
        ]
    }

    // AC1 + AC3: tool_use -> execution -> reinjection -> loop back -> clean end_turn.
    #[tokio::test]
    async fn loop_runs_tool_then_ends() {
        let provider = ScriptedProvider::new(vec![
            bash_turn("call_1", "echo bonjour-pyxis"),
            text_turn("Voilà, c'est fait."),
        ]);
        let out = run_agent(&provider, None, "fais un echo", 5, Duration::from_secs(5)).await;

        assert_eq!(out.turns, 2, "doit reboucler exactement une fois");
        assert!(
            matches!(out.ended, EndState::EndTurn),
            "fin propre attendue"
        );
        assert_eq!(out.invocations.len(), 1);
        assert_eq!(out.invocations[0].output, "bonjour-pyxis");
        assert!(
            out.invocations[0].untrusted,
            "sortie outil = untrusted par défaut"
        );
        assert_eq!(out.final_text, "Voilà, c'est fait.");
    }

    // AC2: a tool exceeding the timeout does not freeze the loop.
    #[tokio::test]
    async fn tool_timeout_does_not_freeze_loop() {
        let provider = ScriptedProvider::new(vec![
            bash_turn("call_1", "sleep 5"),
            text_turn("repris la main."),
        ]);
        let out = run_agent(&provider, None, "dors", 5, Duration::from_millis(200)).await;

        assert!(out.invocations[0].timed_out, "le timeout doit être signalé");
        assert!(
            matches!(out.ended, EndState::EndTurn),
            "la boucle continue et se ferme"
        );
        assert_eq!(out.turns, 2);
    }

    #[test]
    fn decide_transition_is_exhaustive_and_pure() {
        let mut acc = Accumulator::new();
        acc.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
        });
        assert!(matches!(decide_transition(&acc), Transition::EndTurn));

        let mut acc = Accumulator::new();
        acc.push(StreamEvent::ToolCallStart {
            id: "x".into(),
            name: "bash".into(),
        });
        acc.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
        });
        assert!(matches!(decide_transition(&acc), Transition::RunTools(_)));

        let mut acc = Accumulator::new();
        acc.push(StreamEvent::Done {
            stop: StopReason::MaxTokens,
        });
        assert!(matches!(decide_transition(&acc), Transition::Exhausted(_)));
    }

    #[tokio::test]
    async fn missing_cmd_is_handled_without_panic() {
        let inv = exec_bash("bash", "not json", Duration::from_secs(1)).await;
        assert!(inv.output.contains("manquant"));
        assert!(!inv.timed_out);
    }
}
