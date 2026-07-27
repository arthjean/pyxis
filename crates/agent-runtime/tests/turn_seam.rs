//! US-001 AC2: a prototype drives `run_agent` through `TurnRunner`, with a fake
//! provider and a fake store, and re-implements NOTHING the engine already owns.
//!
//! The proof is negative on purpose: the runner code contains no retry, no
//! compaction and no dispatch, yet the turn observed through the seam retries a
//! mid-stream failure, runs a tool through the injected dispatcher and ends on a
//! single terminal event.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::clock::Clock;
use agent_core::compaction::CompactKind;
use agent_core::event::AgentEvent;
use agent_core::message::{ContentBlock, Message};
use agent_core::provider::{
    CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderKind, StopReason, StreamEvent,
};
use agent_core::session::{FileSnapshot, Session, SessionError};
use agent_core::tools::{ToolDispatch, ToolEventSink, ToolInvocation, ToolOutcome};
use agent_core::{AgentContext, CancelToken, Deps, RunConfig};
use agent_runtime::id::{SequentialIds, TurnId};
use agent_runtime::runner::{RunAgentRunner, TurnOutcome, TurnRequest, TurnRunner};
use agent_tokenizer::HeuristicCounter;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ───────── fakes ─────────

enum Scripted {
    Stream(Vec<StreamEvent>),
    /// Some events THEN a mid-stream failure: the engine is the one that must
    /// reset the stream and retry.
    StreamThenErr(Vec<StreamEvent>, ProviderError),
    /// One event then a stream that never completes: only cancellation ends it.
    StreamThenHang(Vec<StreamEvent>),
}

struct FakeProvider {
    caps: Capabilities,
    turns: Mutex<VecDeque<Scripted>>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    async fn stream(
        &self,
        _req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        match self.turns.lock().unwrap().pop_front() {
            Some(Scripted::Stream(events)) => Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            ))),
            Some(Scripted::StreamThenErr(events, err)) => {
                let mut items: Vec<Result<StreamEvent, ProviderError>> =
                    events.into_iter().map(Ok).collect();
                items.push(Err(err));
                Ok(Box::pin(futures_util::stream::iter(items)))
            }
            Some(Scripted::StreamThenHang(events)) => Ok(Box::pin(
                futures_util::stream::iter(events.into_iter().map(Ok))
                    .chain(futures_util::stream::pending()),
            )),
            None => Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                },
            )]))),
        }
    }
    async fn complete(&self, _req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        Ok(CanonicalResponse {
            content: vec![ContentBlock::Text {
                text: "résumé".into(),
            }],
            usage: Default::default(),
            stop: StopReason::EndTurn,
        })
    }
    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        match err {
            ProviderError::Http { status: 429, .. } => ErrorClass::RateLimited,
            _ => ErrorClass::Retryable,
        }
    }
}

/// Fake store: the seam must not care that persistence is in memory.
#[derive(Default)]
struct FakeStore {
    synced: Mutex<Vec<Message>>,
}

#[async_trait::async_trait]
impl Session for FakeStore {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        *self.synced.lock().unwrap() = messages.to_vec();
        Ok(())
    }
    async fn checkpoint(
        &self,
        _kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        *self.synced.lock().unwrap() = messages.to_vec();
        Ok(())
    }
    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
        Ok(())
    }
    async fn record_file_snapshot(&self, _snapshot: FileSnapshot) -> Result<(), SessionError> {
        Ok(())
    }
}

struct EchoTools;

#[async_trait::async_trait]
impl ToolDispatch for EchoTools {
    async fn dispatch(
        &self,
        calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ToolOutcome> {
        calls
            .into_iter()
            .map(|call| {
                ToolOutcome::new(
                    call.id,
                    format!("dispatched:{}", call.name),
                    false,
                    true,
                    None,
                )
            })
            .collect()
    }
}

struct InstantClock;

#[async_trait::async_trait]
impl Clock for InstantClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
    async fn sleep(&self, _dur: Duration) {}
}

fn deps(provider: Arc<FakeProvider>, store: Arc<FakeStore>) -> Deps {
    Deps {
        provider,
        session: store,
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(InstantClock),
        tools: Arc::new(EchoTools),
        cancel: CancelToken::new(),
    }
}

fn provider(turns: Vec<Scripted>, calls: Arc<Mutex<usize>>) -> Arc<FakeProvider> {
    Arc::new(FakeProvider {
        caps: Capabilities {
            tools: true,
            max_context: 100_000,
            ..Capabilities::default()
        },
        turns: Mutex::new(turns.into()),
        calls,
    })
}

fn context(request: &TurnRequest) -> AgentContext {
    AgentContext::new("test-model")
        .with_config(RunConfig {
            max_retries: 2,
            backoff_base_ms: 0,
            ..RunConfig::default()
        })
        .push(Message::user(request.text.clone()))
}

fn turn_id() -> TurnId {
    TurnId::generate(&SequentialIds::new())
}

async fn collect(mut rx: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        out.push(event);
    }
    out
}

// ───────── tests ─────────

#[tokio::test]
async fn engine_events_cross_the_seam_without_reimplementing_retry_or_dispatch() {
    let calls = Arc::new(Mutex::new(0usize));
    let store = Arc::new(FakeStore::default());
    let provider = provider(
        vec![
            // 1. mid-stream failure -> the ENGINE resets and retries.
            Scripted::StreamThenErr(
                vec![StreamEvent::TextDelta {
                    text: "je commence".into(),
                }],
                ProviderError::Http {
                    status: 429,
                    message: "slow down".into(),
                    retry_after_ms: None,
                },
            ),
            // 2. a tool call -> the ENGINE dispatches through the injected pipeline.
            Scripted::Stream(vec![
                StreamEvent::ToolCallStart {
                    id: "call-1".into(),
                    name: "echo".into(),
                },
                StreamEvent::ToolCallDelta {
                    id: "call-1".into(),
                    args_json: "{}".into(),
                },
                StreamEvent::ToolCallEnd {
                    id: "call-1".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::ToolUse,
                },
            ]),
            // 3. final answer.
            Scripted::Stream(vec![
                StreamEvent::TextDelta {
                    text: "terminé".into(),
                },
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                },
            ]),
        ],
        Arc::clone(&calls),
    );

    let runner = RunAgentRunner::new(deps(provider, Arc::clone(&store)), context);
    let (tx, rx) = mpsc::channel(64);
    let observer = tokio::spawn(collect(rx));
    let outcome = runner
        .run_turn(
            TurnRequest {
                turn_id: turn_id(),
                text: "salut".into(),
            },
            tx,
            CancellationToken::new(),
        )
        .await;
    let events = observer.await.unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(*calls.lock().unwrap(), 3, "the engine retried on its own");
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::StreamReset)),
        "the retry of the engine crossed the seam: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::ToolCall(_))),
        "the tool call crossed the seam"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::ToolResult(view) if view.content == "dispatched:echo")
        ),
        "the injected dispatcher produced the result, not the runtime"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::EndTurn)),
        "exactly one terminal, and it closes the stream: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::EndTurn
                    | AgentEvent::Interrupted
                    | AgentEvent::Error(_)
                    | AgentEvent::Exhausted(_)
            ))
            .count(),
        1
    );

    let persisted = store.synced.lock().unwrap().clone();
    assert!(
        persisted.len() >= 3,
        "the engine persisted the transcript through the fake store: {persisted:?}"
    );
}

#[tokio::test]
async fn the_hierarchical_token_interrupts_the_engine_through_the_seam() {
    let calls = Arc::new(Mutex::new(0usize));
    let store = Arc::new(FakeStore::default());
    let provider = provider(
        vec![Scripted::StreamThenHang(vec![StreamEvent::TextDelta {
            text: "je réfléchis".into(),
        }])],
        Arc::clone(&calls),
    );

    let runner = RunAgentRunner::new(deps(provider, store), context);
    let parent = CancellationToken::new();
    let turn_token = parent.child_token();
    let (tx, mut rx) = mpsc::channel(64);

    let cancel_after_first_event = tokio::spawn({
        let turn_token = turn_token.clone();
        async move {
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                if matches!(event, AgentEvent::Text(_)) && !turn_token.is_cancelled() {
                    turn_token.cancel();
                }
                seen.push(event);
            }
            seen
        }
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        runner.run_turn(
            TurnRequest {
                turn_id: turn_id(),
                text: "salut".into(),
            },
            tx,
            turn_token,
        ),
    )
    .await
    .expect("a cancelled turn must reach its terminal quickly");
    let events = cancel_after_first_event.await.unwrap();

    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(
        matches!(events.last(), Some(AgentEvent::Interrupted)),
        "the engine emitted its own terminal after reconciling: {events:?}"
    );
    assert!(
        !parent.is_cancelled(),
        "cancelling a turn must not reach its parent domain"
    );
}
