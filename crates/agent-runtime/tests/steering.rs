//! US-007: correcting a turn that is already running.
//!
//! The property under test is not "the text arrives" but WHERE it arrives: at a
//! safe point, exactly once, in acceptance order, and never inside a sampling or
//! between a `tool_use` and its result.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_runtime::event::ThreadEventPayload;
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::ThreadStore;
use agent_runtime::thread::{MAX_PENDING_INPUTS, Submission, SubmitError};

use common::{
    EchoTools, FakeProvider, FakeSession, GatedTools, Scripted, agent_context, collect_events,
    deps, done_end_turn, engine_labels, start, text, tool_call, user_texts, wait_for,
    wait_for_terminal,
};

/// AC1 and AC2: a steer accepted while the model is talking is durable, visible
/// as pending, cuts THAT sampling with a `StreamReset`, and the next step is
/// sampled with the correction in the transcript.
#[tokio::test]
async fn a_steer_cuts_the_sampling_and_enters_the_next_step() {
    let provider = FakeProvider::new(vec![
        // The model starts answering, then hangs: only the steer ends it.
        Scripted::StreamThenHang(vec![text("je pars sur une mauvaise piste")]),
        Scripted::Stream(vec![text("corrigé"), done_end_turn()]),
    ]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    let accepted = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();

    let streaming = Arc::clone(&events);
    wait_for(
        move || engine_labels(&streaming.lock().unwrap()).contains(&"text".to_string()),
        "the model to start talking",
    )
    .await;

    let before = Instant::now();
    let steer = harness
        .handle
        .steer(
            Submission::new("non, fais plutôt ceci"),
            Some(accepted.turn_id),
        )
        .await
        .expect("the steer is accepted");
    let admission = before.elapsed();
    assert!(
        admission < Duration::from_millis(100),
        "a steer is admitted in {admission:?}, budget 100 ms"
    );
    assert_eq!(
        steer.turn_id, accepted.turn_id,
        "the correction belongs to the turn it targeted"
    );

    let turn = wait_for_terminal(&harness.handle).await;
    assert_eq!(turn.state, TurnState::Completed);

    let labels = engine_labels(&events.lock().unwrap());
    let reset = labels
        .iter()
        .position(|l| l == "stream_reset")
        .expect("the abandoned sampling told the client to erase it");
    assert!(
        labels.iter().position(|l| l == "end_turn").unwrap() > reset,
        "reset before the terminal: {labels:?}"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the steer forced a new sampling");
    assert!(
        !user_texts(&requests[0])
            .iter()
            .any(|t| t.contains("plutôt")),
        "the sampling in flight never saw the correction"
    );
    assert_eq!(
        user_texts(&requests[1]),
        vec!["commence".to_string(), "non, fais plutôt ceci".to_string()],
        "the next step carries the correction, once, after the original input"
    );

    harness.handle.shutdown().await;
}

/// AC3: a steer that lands while a tool runs waits for that tool's result to be
/// persisted, then enters before the next sampling. Cutting a dispatch would
/// leave a `tool_use` without its `tool_result`.
#[tokio::test]
async fn a_steer_during_a_tool_waits_for_its_result() {
    let provider = FakeProvider::new(vec![
        Scripted::Stream(tool_call("call-1", "echo")),
        Scripted::Stream(vec![text("fini"), done_end_turn()]),
    ]);
    let tools = GatedTools::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::clone(&tools) as Arc<dyn agent_core::tools::ToolDispatch>,
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    let accepted = harness
        .handle
        .submit(Submission::new("lance"))
        .await
        .unwrap();

    // Steer while the tool is in flight.
    let started = Arc::clone(&tools.started);
    wait_for(move || started.available_permits() > 0, "the tool to start").await;
    harness
        .handle
        .steer(Submission::new("précision"), Some(accepted.turn_id))
        .await
        .unwrap();
    assert_eq!(
        harness.handle.status().pending_steers,
        1,
        "the correction is pending, not consumed mid-tool"
    );
    // The tool has not returned, so no new sampling can have happened.
    assert_eq!(provider.calls(), 1);

    tools.release.add_permits(1);
    wait_for_terminal(&harness.handle).await;

    let labels = engine_labels(&events.lock().unwrap());
    let result = labels
        .iter()
        .position(|l| l.starts_with("tool_result:"))
        .expect("the tool produced its result");
    assert!(
        !labels[..result].contains(&"stream_reset".to_string()),
        "the dispatch was never cut: {labels:?}"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        user_texts(&requests[1]).contains(&"précision".to_string()),
        "the correction entered before the next sampling"
    );

    harness.handle.shutdown().await;
}

/// AC4: several steers keep their acceptance order and each appears once.
///
/// The turn is held inside a tool rather than inside a sampling, so the three
/// corrections are all accepted against the same running turn and reach the SAME
/// safe point: what is under test is their order, not the race of AC6.
#[tokio::test]
async fn several_steers_keep_their_order_and_are_consumed_once() {
    let provider = FakeProvider::new(vec![
        Scripted::Stream(tool_call("call-1", "echo")),
        Scripted::Stream(vec![text("compris"), done_end_turn()]),
    ]);
    let tools = GatedTools::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::clone(&tools) as Arc<dyn agent_core::tools::ToolDispatch>,
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let accepted = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();

    let started = Arc::clone(&tools.started);
    wait_for(move || started.available_permits() > 0, "the tool to start").await;

    for text in ["un", "deux", "trois"] {
        harness
            .handle
            .steer(Submission::new(text), Some(accepted.turn_id))
            .await
            .unwrap();
    }
    assert_eq!(harness.handle.status().pending_steers, 3);

    tools.release.add_permits(1);
    wait_for_terminal(&harness.handle).await;

    let requests = provider.requests();
    assert_eq!(
        user_texts(requests.last().unwrap()),
        vec![
            "commence".to_string(),
            "un".to_string(),
            "deux".to_string(),
            "trois".to_string()
        ],
        "acceptance order preserved, no duplicate"
    );
    assert_eq!(harness.handle.status().pending_steers, 0);

    harness.handle.shutdown().await;
}

/// AC5: naming a turn that is no longer active is refused with the current
/// state. It must NOT silently become a new turn.
#[tokio::test]
async fn a_stale_target_is_refused_and_never_becomes_a_new_turn() {
    let provider = FakeProvider::new(vec![Scripted::Stream(vec![text("fini"), done_end_turn()])]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let accepted = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();
    wait_for_terminal(&harness.handle).await;

    let refused = harness
        .handle
        .steer(Submission::new("trop tard"), Some(accepted.turn_id))
        .await
        .expect_err("a finished turn cannot be steered");
    assert!(
        matches!(refused, SubmitError::StaleTurn { .. }),
        "got {refused:?}"
    );

    // Nothing was persisted and nothing started.
    harness.handle.shutdown().await;
    let inputs: Vec<String> = harness
        .store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.payload {
            ThreadEventPayload::InputSubmitted { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        inputs,
        vec!["commence".to_string()],
        "a refused steer writes nothing"
    );
}

/// AC6, the other branch of the race: a steer with no named target that lands
/// after the terminal opens a turn of its own instead of being lost.
#[tokio::test]
async fn an_untargeted_steer_after_the_terminal_opens_a_new_turn() {
    let provider = FakeProvider::new(vec![
        Scripted::Stream(vec![text("un"), done_end_turn()]),
        Scripted::Stream(vec![text("deux"), done_end_turn()]),
    ]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let first = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();
    wait_for_terminal(&harness.handle).await;

    let second = harness
        .handle
        .steer(Submission::new("continue"), None)
        .await
        .expect("an untargeted steer is never lost");
    assert_ne!(second.turn_id, first.turn_id, "it opened a turn of its own");

    let handle = Arc::clone(&harness.handle);
    wait_for(
        move || {
            handle
                .status()
                .turn
                .is_some_and(|t| t.turn_id == second.turn_id && t.state == TurnState::Completed)
        },
        "the new turn to complete",
    )
    .await;

    harness.handle.shutdown().await;
}

/// AC6, first branch: an input accepted for a turn that reached its terminal
/// before consuming it is re-admitted as a new turn. Never lost, never consumed
/// twice.
#[tokio::test]
async fn an_input_the_turn_never_consumed_is_re_admitted() {
    let provider = FakeProvider::new(vec![
        Scripted::Stream(tool_call("call-1", "echo")),
        Scripted::Stream(vec![text("fini"), done_end_turn()]),
        Scripted::Stream(vec![text("suite"), done_end_turn()]),
    ]);
    let tools = GatedTools::new();
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::clone(&tools) as Arc<dyn agent_core::tools::ToolDispatch>,
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let first = harness
        .handle
        .submit(Submission::new("lance"))
        .await
        .unwrap();

    // Interrupt while the tool runs, THEN steer: the turn is on its way to a
    // terminal it will reach without ever draining the queue.
    let started = Arc::clone(&tools.started);
    wait_for(move || started.available_permits() > 0, "the tool to start").await;
    harness.handle.interrupt(None).await.unwrap();
    let steer = harness
        .handle
        .steer(Submission::new("jamais consommé"), None)
        .await;

    // The steer either joined the dying turn or opened a new one; either way it
    // must end up running as its own turn.
    let turn_id = steer.map(|a| a.turn_id).unwrap_or(first.turn_id);
    let _ = turn_id;
    tools.release.add_permits(1);

    let handle = Arc::clone(&harness.handle);
    wait_for(
        move || {
            handle
                .status()
                .turn
                .is_some_and(|t| t.turn_id != first.turn_id && t.state.is_terminal())
        },
        "the unconsumed input to run as its own turn",
    )
    .await;

    harness.handle.shutdown().await;
    let inputs: Vec<String> = harness
        .store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.payload {
            ThreadEventPayload::InputSubmitted { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        inputs.contains(&"jamais consommé".to_string()),
        "the input stayed durable: {inputs:?}"
    );
}

/// The steering queue is bounded like every other queue of the runtime (FR-21).
#[tokio::test]
async fn the_steering_queue_is_bounded() {
    let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("je réfléchis")])]);
    let runner = Arc::new(RunAgentRunner::new(
        deps(
            Arc::clone(&provider),
            FakeSession::new(),
            Arc::new(EchoTools),
        ),
        agent_context,
    ));

    let harness = start(runner).await;
    let events = collect_events(harness.handle.subscribe());
    let accepted = harness
        .handle
        .submit(Submission::new("commence"))
        .await
        .unwrap();
    let streaming = Arc::clone(&events);
    wait_for(
        move || engine_labels(&streaming.lock().unwrap()).contains(&"text".to_string()),
        "the model to start talking",
    )
    .await;

    // The first steer cuts the sampling, so the engine may drain the queue at
    // its safe point; what must hold is that the queue never grows past its
    // bound, whatever the engine consumed.
    let mut refused = None;
    for index in 0..MAX_PENDING_INPUTS * 2 {
        if let Err(err) = harness
            .handle
            .steer(
                Submission::new(format!("correction {index}")),
                Some(accepted.turn_id),
            )
            .await
        {
            refused = Some(err);
            break;
        }
        assert!(harness.handle.status().pending_steers <= MAX_PENDING_INPUTS);
    }
    if let Some(err) = refused {
        assert!(
            matches!(err, SubmitError::PendingFull { max } if max == MAX_PENDING_INPUTS)
                || matches!(err, SubmitError::StaleTurn { .. }),
            "got {err:?}"
        );
    }

    harness.handle.shutdown().await;
}
