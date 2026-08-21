//! Scripted provider for the transcript harness (US-122).
//!
//! Same shape as the one `crates/agent-cli/tests/e2e_headless.rs` carries, and
//! for the same reason: the outgoing request is composed by the real
//! `build_responses_body` and the recorded stream is decoded by the real
//! `CodexEventMapper`, so what the harness exercises is the provider contract,
//! not a mock of it. Duplicated rather than shared because `agent-cli` has no
//! `[lib]` target: a `#[cfg(test)] mod` under `src/` cannot import from
//! `tests/`, and the fixtures are reachable only through `include_str!`.
//!
//! Not a byte goes on the network. No socket is opened, no endpoint is named,
//! no credential is read: the two failure modes this provider models are "the
//! script ran out" and "the script was never finished", and both are decided in
//! memory.

use std::collections::VecDeque;
use std::sync::Mutex;

use agent_core::provider::{
    CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderKind, StreamEvent,
};
use agent_provider::CodexEventMapper;
use agent_provider::chatgpt_request::{ResponsesBodyOptions, build_responses_body};
use futures_util::stream::BoxStream;

/// Extracts the `data:` payloads of a recorded SSE stream, in order.
fn sse_payloads(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|payload| payload.trim().to_string())
        .filter(|payload| !payload.is_empty())
        .collect()
}

/// One recorded stream per model turn, played in order.
pub struct ScriptedProvider {
    /// Named in every failure this provider raises: a scenario that drifts is
    /// only useful to debug when the message says WHICH scenario drifted.
    ///
    /// Owned rather than `&'static str`: a scenario is discovered by scanning a
    /// directory (US-126), so neither its name nor its recorded streams exist at
    /// compile time.
    scenario: String,
    remaining: Mutex<VecDeque<ScriptEntry>>,
    /// The composed outgoing bodies, kept for inspection. The only outgoing
    /// contract of a run, so what proves no credential travels is reading them.
    bodies: Mutex<Vec<serde_json::Value>>,
    /// 1-based count of requests received, the out-of-script ones included.
    /// It is the rank the error names, so the message points at the request
    /// that had no answer rather than at the last one that did.
    requests: Mutex<u32>,
    capabilities: Capabilities,
}

struct ScriptEntry {
    name: String,
    sse: String,
}

impl ScriptedProvider {
    pub fn new(
        scenario: impl Into<String>,
        script: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            scenario: scenario.into(),
            remaining: Mutex::new(
                script
                    .into_iter()
                    .map(|(name, sse)| ScriptEntry { name, sse })
                    .collect(),
            ),
            bodies: Mutex::new(Vec::new()),
            requests: Mutex::new(0),
            capabilities: Capabilities {
                tools: true,
                max_context: 128_000,
                ..Capabilities::default()
            },
        }
    }

    pub fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// US-122: a script left half played is a scenario that stopped early, and
    /// every assertion the test made before this point was made on half a run.
    /// Silent when the queue is empty, and naming what stayed in it otherwise.
    pub fn assert_consumed(&self) {
        let held = self
            .remaining
            .lock()
            .expect("the script lock is not poisoned");
        let names: Vec<&str> = held.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            names.is_empty(),
            "scenario `{}`: the script was not consumed, {} never played",
            self.scenario,
            names.join(", ")
        );
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        // The real composition path: it carries the guardrail against orphan
        // tool calls, so a broken transcript fails here and not in a mock.
        let body = build_responses_body(&req, ResponsesBodyOptions::default());
        if let Ok(mut held) = self.bodies.lock() {
            held.push(body);
        }

        let rank = {
            let mut count = self
                .requests
                .lock()
                .map_err(|_| ProviderError::Transport("scripted provider: poisoned".into()))?;
            *count += 1;
            *count
        };
        let entry = self
            .remaining
            .lock()
            .map_err(|_| ProviderError::Transport("scripted provider: poisoned".into()))?
            .pop_front();
        // An empty stream would read as a well-formed turn that said nothing,
        // and the scenario would go green one request past its script. The
        // named error is what makes that drift visible.
        let Some(entry) = entry else {
            return Err(ProviderError::Transport(format!(
                "scenario `{}`: request #{rank} is beyond the script",
                self.scenario
            )));
        };

        // REAL decoding of the recorded stream, the "a stream without a
        // terminal event is a contract error" rule of the ChatGPT provider
        // included.
        let mut mapper = CodexEventMapper::new();
        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();
        let mut saw_terminal = false;
        for payload in sse_payloads(&entry.sse) {
            match mapper.ingest(&payload) {
                Ok(decoded) => {
                    for event in decoded {
                        saw_terminal |= matches!(event, StreamEvent::Done { .. });
                        events.push(Ok(event));
                    }
                }
                Err(err) => {
                    events.push(Err(err));
                    saw_terminal = true;
                    break;
                }
            }
        }
        if !saw_terminal {
            events.push(Err(ProviderError::Stream("missing terminal event".into())));
        }
        Ok(Box::pin(futures_util::stream::iter(events)))
    }

    async fn complete(&self, _req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        // Compaction is never reached by these scenarios (short context).
        Err(ProviderError::Transport(format!(
            "scenario `{}`: complete() is out of scope",
            self.scenario
        )))
    }

    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        match err {
            // A cut fixture is retryable and consumes the next recorded stream.
            ProviderError::Stream(message) if message == "missing terminal event" => {
                ErrorClass::Retryable
            }
            _ => ErrorClass::InvalidRequest,
        }
    }
}
