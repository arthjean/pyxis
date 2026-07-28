//! US-006: the turn is frozen, the step is refreshed.
//!
//! The unit tests of `agent_runtime::context` prove how a step is BUILT (order,
//! bounds, fallbacks). What is proven here is that the engine actually consumes
//! one frame PER MODEL REQUEST: the catalog of a sampling in flight cannot move
//! under it, and the next request sees the new generation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use agent_core::provider::ToolSpec;
use agent_runtime::context::{
    StepContexts, StepSection, StepSnapshot, StepSource, TurnContextSource,
};
use agent_runtime::id::{RandomIds, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::thread::Submission;

use common::{
    EchoTools, FakeProvider, FakeSession, Scripted, agent_context, deps, done_end_turn, start,
    text, tool_call, turn_context, user_texts, wait_for_terminal,
};

/// A source the test moves between two steps.
struct Movable(Mutex<StepSnapshot>);

impl Movable {
    fn new(snapshot: StepSnapshot) -> Arc<Self> {
        Arc::new(Self(Mutex::new(snapshot)))
    }
    fn set(&self, snapshot: StepSnapshot) {
        *self.0.lock().unwrap() = snapshot;
    }
}

impl StepSource for Movable {
    fn snapshot(&self) -> StepSnapshot {
        self.0.lock().unwrap().clone()
    }
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "outil de test".into(),
        // The strict shape `CanonicalRequest::validate` demands: without it the
        // turn would fail before ever reaching the provider.
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false,
        }),
    }
}

fn snapshot(tools: Vec<ToolSpec>, environment: &str) -> StepSnapshot {
    StepSnapshot {
        tools,
        sections: vec![
            StepSection::stable("agents", Some("# règles projet".into())),
            StepSection::volatile("environment", Some(environment.into())),
        ],
    }
}

/// AC2 and AC3: every request carries a captured step, and two steps whose
/// source did not move inject byte-identical context in the same order.
#[tokio::test]
async fn two_steps_without_a_source_change_inject_the_same_prefix() {
    let source = Movable::new(snapshot(vec![tool("read")], "<cwd>/tmp</cwd>"));
    let steps = Arc::new(StepContexts::new(
        Arc::clone(&source) as Arc<dyn StepSource>,
        Arc::new(RandomIds),
    ));

    // Two model requests: a tool call, then the final answer.
    let provider = FakeProvider::new(vec![
        Scripted::Stream(tool_call("call-1", "read")),
        Scripted::Stream(vec![text("fini"), done_end_turn()]),
    ]);
    let factory_steps = Arc::clone(&steps);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        move |request: &_| agent_context(request).with_step_source(Arc::clone(&factory_steps) as _),
    ));

    let harness = start(runner).await;
    harness
        .handle
        .submit(Submission::new("salut"))
        .await
        .unwrap();
    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.state, TurnState::Completed);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "two steps were sampled");
    // The context sections open every request, in the same order and with the
    // same bytes: the cacheable prefix did not move between the two steps.
    let first = user_texts(&requests[0]);
    let second = user_texts(&requests[1]);
    assert_eq!(
        first[..2],
        second[..2],
        "stable then volatile, byte-identical"
    );
    assert_eq!(first[0], "# règles projet");
    assert_eq!(first[1], "<cwd>/tmp</cwd>");
    assert_eq!(requests[0].tools, requests[1].tools);
    assert_eq!(
        steps.last().map(|s| s.generation),
        Some(1),
        "an unchanged source never bumps the generation"
    );

    harness.handle.shutdown().await;
}

/// AC4: a catalog staged WHILE a sampling runs does not reach that sampling. The
/// step in flight keeps what it was built with; the next step sees the new
/// generation.
#[tokio::test]
async fn a_catalog_staged_during_a_sampling_only_reaches_the_next_step() {
    let source = Movable::new(snapshot(vec![tool("read")], "<cwd>/tmp</cwd>"));
    let steps = Arc::new(StepContexts::new(
        Arc::clone(&source) as Arc<dyn StepSource>,
        Arc::new(RandomIds),
    ));

    let provider = FakeProvider::new(vec![
        Scripted::Stream(tool_call("call-1", "read")),
        Scripted::Stream(vec![text("fini"), done_end_turn()]),
    ]);
    // The MCP server connects while the first sampling is in flight: the request
    // was already built, so this must not change it.
    let staged = Arc::clone(&source);
    provider.on_open(Arc::new(move |index| {
        if index == 1 {
            staged.set(snapshot(
                vec![tool("read"), tool("mcp__search")],
                "<cwd>/tmp</cwd>",
            ));
        }
    }));

    let factory_steps = Arc::clone(&steps);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        move |request: &_| agent_context(request).with_step_source(Arc::clone(&factory_steps) as _),
    ));

    let harness = start(runner).await;
    harness
        .handle
        .submit(Submission::new("salut"))
        .await
        .unwrap();
    wait_for_terminal(&harness.handle).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let names = |request: &agent_core::provider::CanonicalRequest| -> Vec<String> {
        request.tools.iter().map(|t| t.name.clone()).collect()
    };
    assert_eq!(
        names(&requests[0]),
        vec!["read".to_string()],
        "the sampling in flight kept its catalog"
    );
    assert_eq!(
        names(&requests[1]),
        vec!["read".to_string(), "mcp__search".to_string()],
        "the next step sees the staged server"
    );
    assert_eq!(
        steps.last().map(|s| s.generation),
        Some(2),
        "the generation moved exactly once"
    );

    harness.handle.shutdown().await;
}

/// AC1: the turn keeps the configuration it was captured with. Moving the source
/// while the turn runs changes the NEXT turn, never the current one.
#[tokio::test]
async fn a_turn_keeps_the_context_it_was_captured_with() {
    let provider = FakeProvider::new(vec![
        Scripted::Stream(vec![text("un"), done_end_turn()]),
        Scripted::Stream(vec![text("deux"), done_end_turn()]),
    ]);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        move |request: &agent_runtime::runner::TurnRequest| {
            sink.lock().unwrap().push(request.context.model.clone());
            agent_context(request)
        },
    ));

    let harness = start(runner).await;
    let accepted = harness
        .handle
        .submit(Submission::new("premier"))
        .await
        .unwrap();
    wait_for_terminal(&harness.handle).await;

    // The frozen capture carries the turn that asked for it.
    let first = harness
        .contexts
        .capture(accepted.turn_id)
        .expect("context captures")
        .context;
    assert_eq!(first.turn_id, accepted.turn_id);
    assert_eq!(first.model, "test-model");

    // A settings change between two turns reaches the second turn only.
    let mut next = turn_context(TurnId::generate(&RandomIds));
    next.model = "autre-modele".into();
    next.permission_mode = "auto".into();
    harness.contexts.set(next);

    harness
        .handle
        .submit(Submission::new("second"))
        .await
        .unwrap();
    wait_for_terminal(&harness.handle).await;

    assert_eq!(
        captured.lock().unwrap().clone(),
        vec!["test-model".to_string(), "autre-modele".to_string()],
        "the first turn ran on its own capture from start to terminal"
    );

    harness.handle.shutdown().await;
}
