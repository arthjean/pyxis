//! The four critical races of the runtime, repeated (US-019 AC1).
//!
//! Each race is run [`REPETITIONS`] times with a deterministic clock and a
//! deterministic identifier generator, so a failure is reproducible and a pass
//! is not one lucky interleaving. What is asserted is never "it did not crash"
//! but the invariant the PRD names: no input lost, no terminal duplicated, no
//! lease left occupied, no task left running.
//!
//! Timing variability comes from the iteration index rather than from a sleep: a
//! test that waits for a race to happen by chance is a test that spends its time
//! sleeping and still misses the interleaving it was written for.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_core::provider::StreamEvent;
use agent_core::tools::{ModelToolResult, ToolDispatch, ToolEventSink, ToolInvocation};
use agent_runtime::agent::{AgentAuthority, MAX_ACTIVE_AGENTS};
use agent_runtime::context::{FixedTurnContext, TurnContextSource};
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::id::{SequentialIds, ThreadId, TurnId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::supervisor::AgentSupervisor;
use agent_runtime::thread::{Submission, ThreadHandle, ThreadOptions};
use tokio_util::sync::CancellationToken;

mod common;
use common::{
    ChildScript, FakeProvider, FakeSession, InstantClock, Scripted, ScriptedSpawner, agent_context,
    deps, done_end_turn, text, tool_call, turn_context, user_texts, wait_for_terminal,
};

/// Repetitions of each race. The PRD's reliability target is "zero loss over
/// 1 000 repetitions of each of the four critical races".
const REPETITIONS: usize = 1_000;

/// Yields a number of times derived from the iteration index, so the operation
/// under test lands at a different point of the actor's schedule each round
/// without a single sleep.
async fn jitter(round: usize) {
    for _ in 0..(round % 4) {
        tokio::task::yield_now().await;
    }
}

/// A thread whose clock and identifiers are deterministic.
struct Race {
    handle: Arc<ThreadHandle>,
    store: Arc<MemoryThreadStore>,
    root: CancellationToken,
}

async fn start_race(
    runner: Arc<dyn agent_runtime::runner::TurnRunner>,
    agents: Option<Arc<AgentSupervisor>>,
    seed: u64,
) -> Race {
    let ids = Arc::new(SequentialIds::starting_at(seed));
    let store = Arc::new(MemoryThreadStore::new());
    let thread_id = ThreadId::generate(ids.as_ref());
    let root = CancellationToken::new();
    let handle = ThreadHandle::start(ThreadOptions {
        thread_id,
        store: Arc::clone(&store) as Arc<dyn ThreadStore>,
        runner,
        turn_contexts: Arc::new(FixedTurnContext::new(turn_context(TurnId::generate(
            ids.as_ref(),
        )))) as Arc<dyn TurnContextSource>,
        ids,
        clock: Arc::new(InstantClock),
        parent_cancel: root.clone(),
        agents,
    })
    .await
    .expect("the thread starts");
    Race {
        handle: Arc::new(handle),
        store,
        root,
    }
}

/// Terminal transitions per turn, as the DURABLE log carries them.
///
/// This is the shape every race checks: a turn that reached a terminal state
/// reached exactly one, and the log is what says so, not the in-memory actor.
async fn terminals(store: &MemoryThreadStore) -> Vec<(TurnId, TurnState)> {
    store
        .read()
        .await
        .expect("the store reads")
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            ThreadEventPayload::TurnStateChanged { turn_id, to, .. } if to.is_terminal() => {
                Some((*turn_id, *to))
            }
            _ => None,
        })
        .collect()
}

fn assert_one_terminal_per_turn(terminals: &[(TurnId, TurnState)], context: &str) {
    let mut seen: Vec<TurnId> = Vec::new();
    for (turn_id, _) in terminals {
        assert!(
            !seen.contains(turn_id),
            "{context}: turn {turn_id} reached a terminal state twice"
        );
        seen.push(*turn_id);
    }
}

/// EP-005 vertical race: the first sampling fails after a visible delta, the
/// second opening races its terminal against cancellation. Every deterministic
/// schedule must produce one terminal and two provider effects, never a third.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_retry_cancel_has_one_terminal_across_1000_schedules() {
    for round in 0..REPETITIONS {
        let provider = FakeProvider::new(vec![
            Scripted::StreamThenErr(
                vec![text("abandoned")],
                agent_core::provider::ProviderError::Stream("cut".into()),
            ),
            Scripted::Stream(vec![text("final"), done_end_turn()]),
        ]);
        let second_opened = Arc::new(tokio::sync::Semaphore::new(0));
        let signal = Arc::clone(&second_opened);
        provider.on_open(Arc::new(move |index| {
            if index == 2 {
                signal.add_permits(1);
            }
        }));
        let runner = Arc::new(RunAgentRunner::new(
            deps(
                Arc::clone(&provider),
                FakeSession::new(),
                Arc::new(common::EchoTools),
            ),
            agent_context,
        ));
        let race = start_race(runner, None, 500_000 + round as u64 * 16).await;

        race.handle
            .submit(Submission::new("retry puis termine"))
            .await
            .expect("the turn opens");
        let permit = tokio::time::timeout(Duration::from_secs(5), second_opened.acquire())
            .await
            .expect("second attempt must open before the timeout")
            .unwrap();
        permit.forget();
        jitter(round).await;
        race.handle.interrupt(None).await.expect("interrupt");
        let terminal = wait_for_terminal(&race.handle).await;
        assert!(
            matches!(
                terminal.state,
                TurnState::Completed | TurnState::Interrupted
            ),
            "round {round}: unexpected terminal {terminal:?}"
        );
        race.handle.shutdown().await;

        assert_eq!(
            provider.calls(),
            2,
            "round {round}: retry/cancel opened the provider more than twice"
        );
        let durable = terminals(&race.store).await;
        assert_eq!(durable.len(), 1, "round {round}: {durable:?}");
        assert_one_terminal_per_turn(&durable, &format!("round {round}"));
        race.root.cancel();
    }
}

// ───────── race 1: steer vs terminal ─────────

/// A steer accepted while a turn is ending belongs to EXACTLY one of the two
/// branches: the turn it targeted, or a new turn queued after its terminal.
/// Never both (the model would read the correction twice), never neither (the
/// user typed something that vanished).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_racing_a_terminal_is_delivered_exactly_once() {
    for round in 0..REPETITIONS {
        let provider = FakeProvider::new(vec![
            Scripted::Stream(vec![text("premier"), done_end_turn()]),
            Scripted::Stream(vec![text("second"), done_end_turn()]),
        ]);
        let runner = Arc::new(RunAgentRunner::new(
            deps(
                Arc::clone(&provider),
                FakeSession::new(),
                Arc::new(common::EchoTools),
            ),
            agent_context,
        ));
        let race = start_race(runner, None, 1_000 + round as u64).await;

        race.handle
            .submit(Submission::new("ouvre le tour"))
            .await
            .expect("the turn opens");
        jitter(round).await;
        // `None` accepts either branch of the race, which is what a client that
        // just wants its input delivered asks for.
        let accepted = race
            .handle
            .steer(Submission::new("correction"), None)
            .await
            .expect("the steer is accepted");

        // Everything settles: the turn it steered, or the turn it opened.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = race.handle.status();
            if status.pending_inputs == 0
                && status.pending_steers == 0
                && status.turn.is_some_and(|turn| turn.state.is_terminal())
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "round {round} hung");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        race.handle.shutdown().await;

        let delivered = provider
            .requests()
            .iter()
            .filter(|request| user_texts(request).iter().any(|text| text == "correction"))
            .count();
        assert_eq!(
            delivered, 1,
            "round {round}: the correction reached the model {delivered} time(s), expected once \
             (accepted as {accepted:?})"
        );
        assert_one_terminal_per_turn(&terminals(&race.store).await, &format!("round {round}"));
        race.root.cancel();
    }
}

// ───────── race 2: interrupt vs a running tool ─────────

/// Hangs inside the tool phase, announces that it got there and reports when
/// its future is DROPPED. The drop is what runs the destructors that terminate a
/// process tree, so "the tool stopped" is a fact about the destructor, not about
/// the event that announced the stop.
struct TrackedHang {
    entered: Arc<tokio::sync::Semaphore>,
    started: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

struct DropMark(Arc<AtomicUsize>);

impl Drop for DropMark {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolDispatch for TrackedHang {
    async fn dispatch(
        &self,
        _calls: Vec<ToolInvocation>,
        _events: ToolEventSink,
    ) -> Vec<ModelToolResult> {
        let _mark = DropMark(Arc::clone(&self.dropped));
        self.started.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        std::future::pending().await
    }
}

/// Interrupting a turn writes exactly one terminal, and a tool that had started
/// is unwound before that terminal: nothing the tool began outlives the turn
/// that began it.
///
/// Half the rounds wait for the tool phase before signalling, so the race is
/// really exercised; the other half signal at once, which is the case where the
/// interruption beats the provider stream and no tool ever runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interruption_during_a_tool_call_unwinds_it_before_the_terminal() {
    let mut reached_tool_phase = 0usize;
    for round in 0..REPETITIONS {
        let provider = FakeProvider::new(vec![Scripted::Stream(tool_call("call_1", "bash"))]);
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(TrackedHang {
            entered: Arc::clone(&entered),
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        });
        let runner = Arc::new(RunAgentRunner::new(
            deps(Arc::clone(&provider), FakeSession::new(), tools),
            agent_context,
        ));
        let race = start_race(runner, None, 2_000 + round as u64).await;

        race.handle
            .submit(Submission::new("appelle un outil"))
            .await
            .expect("the turn opens");
        if round % 2 == 0 {
            let _ = tokio::time::timeout(Duration::from_secs(5), entered.acquire()).await;
        } else {
            jitter(round).await;
        }
        race.handle.interrupt(None).await.expect("interrupt");
        // A second interruption of the same turn is a no-op, never a second
        // terminal.
        race.handle.interrupt(None).await.expect("interrupt again");

        let terminal = wait_for_terminal(&race.handle).await;
        assert_eq!(
            terminal.state,
            TurnState::Interrupted,
            "round {round}: an interrupted turn must end interrupted"
        );
        race.handle.shutdown().await;

        let started = started.load(Ordering::SeqCst);
        assert!(
            started <= 1,
            "round {round}: the tool phase must run at most once"
        );
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            started,
            "round {round}: a tool that started must be unwound"
        );
        reached_tool_phase += started;
        assert_one_terminal_per_turn(&terminals(&race.store).await, &format!("round {round}"));
        race.root.cancel();
    }
    assert!(
        reached_tool_phase >= REPETITIONS / 4,
        "the race never reached the tool phase ({reached_tool_phase} rounds): it would prove \
         nothing about an interruption DURING a tool call"
    );
}

// ───────── race 3: shutdown vs a filling mailbox ─────────

/// Every submission answered with an acceptance is durable, and every one that
/// was not accepted is refused by name. A shutdown that overlaps the submissions
/// never leaves a phantom acceptance behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_never_acknowledges_an_input_it_did_not_persist() {
    for round in 0..REPETITIONS {
        let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("...")])]);
        let runner = Arc::new(RunAgentRunner::new(
            deps(
                Arc::clone(&provider),
                FakeSession::new(),
                Arc::new(common::EchoTools),
            ),
            agent_context,
        ));
        let race = start_race(runner, None, 3_000 + round as u64).await;
        let handle = Arc::clone(&race.handle);

        let submissions = tokio::spawn({
            let handle = Arc::clone(&handle);
            async move {
                let mut accepted = Vec::new();
                for index in 0..8 {
                    match handle
                        .submit(Submission::new(format!("entrée {index}")))
                        .await
                    {
                        Ok(ack) => accepted.push(ack),
                        // Refused by name: shutting down, queue full, stopped.
                        Err(_) => break,
                    }
                }
                accepted
            }
        });
        jitter(round).await;
        handle.shutdown().await;
        let accepted = submissions.await.expect("the submitter finishes");

        let snapshot = race.store.read().await.expect("the store reads");
        for ack in &accepted {
            assert!(
                snapshot
                    .events
                    .iter()
                    .any(|event| event.event_id == ack.event_id),
                "round {round}: {ack:?} was acknowledged without being durable"
            );
        }
        assert_one_terminal_per_turn(&terminals(&race.store).await, &format!("round {round}"));
        race.root.cancel();
    }
}

// ───────── race 4: two spawns for the last slot ─────────

/// Two concurrent spawns competing for the last free slot: exactly one wins, and
/// the loser leaves no lease behind. A phantom lease is worse than a refusal: it
/// makes the NEXT spawn fail for a child that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_spawns_for_the_last_slot_leave_exactly_one_winner() {
    // One fresh parent per round: the total creation budget of a root thread is
    // eight, so a single parent could not host a thousand races.
    for round in 0..REPETITIONS {
        // Children that never finish their turn, so the slots they hold stay
        // held: without that, the "last slot" would free itself and there would
        // be no race to arbitrate.
        let scripts = (0..MAX_ACTIVE_AGENTS + 1)
            .map(|_| ChildScript::new(vec![Scripted::StreamThenHang(vec![text("...")])]))
            .collect();
        let spawner = ScriptedSpawner::new(scripts);
        let supervisor = AgentSupervisor::new(
            Arc::clone(&spawner) as Arc<dyn agent_runtime::supervisor::AgentSpawner>,
            Arc::new(SequentialIds::starting_at(4_000 + round as u64)),
            Arc::new(InstantClock),
            AgentAuthority::read_only(),
        );
        let provider = FakeProvider::new(vec![Scripted::StreamThenHang(vec![text("...")])]);
        let runner = Arc::new(RunAgentRunner::new(
            deps(
                Arc::clone(&provider),
                FakeSession::new(),
                Arc::new(common::EchoTools),
            ),
            agent_context,
        ));
        let race = start_race(runner, Some(Arc::clone(&supervisor)), 5_000 + round as u64).await;

        for index in 0..MAX_ACTIVE_AGENTS - 1 {
            supervisor
                .spawn(
                    &format!("agent_{index}"),
                    format!("tâche {index}"),
                    &AgentAuthority::read_only(),
                )
                .await
                .expect("the slots before the last one are free");
        }

        let read_only = AgentAuthority::read_only();
        let (first, second) = tokio::join!(
            supervisor.spawn("premier", "premier candidat".to_string(), &read_only),
            async {
                jitter(round).await;
                supervisor
                    .spawn("second", "second candidat".to_string(), &read_only)
                    .await
            }
        );
        let winners = usize::from(first.is_ok()) + usize::from(second.is_ok());
        assert_eq!(
            winners, 1,
            "round {round}: exactly one spawn may take the last slot ({first:?} / {second:?})"
        );
        assert_eq!(
            supervisor.graph().active(),
            MAX_ACTIVE_AGENTS,
            "round {round}: the loser must not leave a lease behind"
        );

        race.handle.shutdown().await;
        race.root.cancel();
        assert!(
            supervisor.graph().active() <= MAX_ACTIVE_AGENTS,
            "round {round}: the bound holds after shutdown too"
        );
    }
}

/// Guards the shape of the shorthands the races rely on: a stream that ends and
/// a stream that hangs are the two behaviors every round is built from.
#[test]
fn the_race_fixtures_describe_what_they_claim() {
    assert!(matches!(done_end_turn(), StreamEvent::Done { .. }));
    assert_eq!(tool_call("id", "bash").len(), 4);
}
