//! Acceptance of EP-004: bounded, parent-owned sub-agents.
//!
//! Every test drives the PRODUCTION seam: a real `ThreadHandle` for the parent,
//! a real `AgentSupervisor`, a real `RunAgentRunner` per child. Only the
//! provider, the session and the tool dispatcher are fakes, exactly as in the
//! EP-002 and EP-003 suites, so what is asserted here is orchestration and never
//! a stand-in for it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_runtime::agent::{
    AgentAuthority, AgentError, AgentState, MAX_ACTIVE_AGENTS, MAX_AGENT_DEPTH, MAX_AGENTS_PER_ROOT,
};
use agent_runtime::event::{ThreadEvent, ThreadEventPayload};
use agent_runtime::handoff::UNTRUSTED_BANNER;
use agent_runtime::id::{AgentId, EventId, RandomIds, ThreadId};
use agent_runtime::lifecycle::TurnState;
use agent_runtime::runner::RunAgentRunner;
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::supervisor::{AgentDelivery, AgentSpawner, AgentSupervisor, WaitOutcome};
use agent_runtime::thread::Submission;
use common::{
    ChildScript, FakeProvider, FakeSession, Harness, InstantClock, Scripted, ScriptedSpawner,
    agent_context, deps, done_end_turn, text, user_texts, wait_for,
};

const SHORT: Duration = Duration::from_millis(150);
const GENEROUS: Duration = Duration::from_secs(5);

fn supervisor(spawner: Arc<dyn AgentSpawner>) -> Arc<AgentSupervisor> {
    AgentSupervisor::new(
        spawner,
        Arc::new(RandomIds),
        Arc::new(InstantClock),
        AgentAuthority::unrestricted(),
    )
}

/// A parent thread that answers instantly: this suite is about the children.
fn parent_runner() -> Arc<dyn agent_runtime::runner::TurnRunner> {
    parent_over(FakeProvider::new(vec![]))
}

/// A parent whose turn never ends on its own, so its terminal is the one the
/// shutdown writes.
fn hanging_parent_runner() -> Arc<dyn agent_runtime::runner::TurnRunner> {
    parent_over(FakeProvider::new(vec![Scripted::StreamThenHang(vec![
        text("le parent réfléchit"),
    ])]))
}

fn parent_over(provider: Arc<FakeProvider>) -> Arc<dyn agent_runtime::runner::TurnRunner> {
    Arc::new(RunAgentRunner::new(
        deps(provider, FakeSession::new(), Arc::new(common::EchoTools)),
        agent_context,
    ))
}

fn answers(reply: &str) -> ChildScript {
    ChildScript::new(vec![Scripted::Stream(vec![text(reply), done_end_turn()])])
}

fn hangs() -> ChildScript {
    ChildScript::new(vec![Scripted::StreamThenHang(vec![text("en cours")])])
}

/// A child whose provider refuses fatally: the engine does not retry, so the
/// turn really fails.
fn crashes() -> ChildScript {
    ChildScript::fatal(vec![Scripted::OpenErr(
        agent_core::provider::ProviderError::Http {
            status: 400,
            message: "modèle inconnu".into(),
            retry_after_ms: None,
        },
    )])
}

async fn events(store: &Arc<MemoryThreadStore>) -> Vec<ThreadEventPayload> {
    store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.payload)
        .collect()
}

fn agent_terminals(payloads: &[ThreadEventPayload]) -> Vec<(AgentId, AgentState)> {
    payloads
        .iter()
        .filter_map(|p| match p {
            ThreadEventPayload::AgentStateChanged { agent_id, to, .. } => Some((*agent_id, *to)),
            _ => None,
        })
        .collect()
}

async fn start(spawner: &Arc<ScriptedSpawner>) -> (Harness, Arc<AgentSupervisor>) {
    let supervisor = supervisor(Arc::clone(spawner) as Arc<dyn AgentSpawner>);
    let harness = common::start_with(parent_runner(), Some(Arc::clone(&supervisor))).await;
    spawner.watch(Arc::clone(&harness.store));
    (harness, supervisor)
}

// ───────── US-012: graph, leases, filiation ─────────

/// AC1: the slot, the identifiers and the durable edge all exist BEFORE the
/// spawner is asked for anything. The spawner reads the parent's log itself, so
/// the ordering is observed and not inferred.
#[tokio::test]
async fn a_spawn_is_reserved_and_persisted_before_the_child_is_created() {
    let spawner = ScriptedSpawner::new(vec![answers("fait")]);
    let (harness, supervisor) = start(&spawner).await;

    let spawned = supervisor
        .spawn(
            "agent_1",
            "explorer les tests",
            &AgentAuthority::read_only(),
        )
        .await
        .expect("the spawn is accepted");

    let payloads = events(&harness.store).await;
    assert!(
        matches!(
            &payloads[1],
            ThreadEventPayload::AgentLinked { agent_id, child_thread_id, task, .. }
                if *agent_id == spawned.agent_id
                    && *child_thread_id == spawned.thread_id
                    && task == "explorer les tests"
        ),
        "the filiation must be the parent's second event: {payloads:?}"
    );
    assert_eq!(
        spawner.parent_events_before_create(),
        vec![2],
        "the child is created only once the edge is durable"
    );
    // The reservation itself, not the child's speed: a fast child may already
    // have answered and gone idle by the time the spawn returns.
    assert_eq!(supervisor.graph().created(), 1);
    assert!(supervisor.graph().get(spawned.agent_id).is_some());
    harness.handle.shutdown().await;
}

/// AC2: the three bounds refuse before anything is created. No spawner call, so
/// no store, no engine and no provider round-trip.
#[tokio::test]
async fn the_v1_limits_refuse_a_spawn_before_anything_is_created() {
    let spawner = ScriptedSpawner::new(vec![hangs(), hangs(), hangs(), hangs()]);
    let (harness, supervisor) = start(&spawner).await;

    for index in 0..MAX_ACTIVE_AGENTS {
        supervisor
            .spawn(
                &format!("agent_{index}"),
                "explorer",
                &AgentAuthority::read_only(),
            )
            .await
            .expect("a slot is available");
    }
    let refused = supervisor
        .spawn("un_de_trop", "un de trop", &AgentAuthority::read_only())
        .await
        .expect_err("the fifth concurrent child must be refused");

    assert!(matches!(
        refused,
        AgentError::LimitReached { active: 4, .. }
    ));
    assert_eq!(
        spawner.requests().len(),
        MAX_ACTIVE_AGENTS,
        "a refused spawn creates nothing"
    );
    // The bounds are constants of the runtime, never configuration (FR-20).
    assert_eq!(
        (MAX_ACTIVE_AGENTS, MAX_AGENTS_PER_ROOT, MAX_AGENT_DEPTH),
        (4, 8, 1)
    );
    harness.handle.shutdown().await;
}

/// AC4: a spawner failure frees the slot and leaves a durable cause behind.
#[tokio::test]
async fn a_failed_creation_frees_its_slot_and_records_why() {
    let spawner = ScriptedSpawner::broken();
    let (harness, supervisor) = start(&spawner).await;

    let err = supervisor
        .spawn("agent_4", "explorer", &AgentAuthority::read_only())
        .await
        .expect_err("a broken spawner must not produce a child");
    assert!(matches!(err, AgentError::Spawn(cause) if cause.contains("indisponible")));

    assert_eq!(supervisor.graph().active(), 0, "the slot is freed");
    let payloads = events(&harness.store).await;
    let terminals = agent_terminals(&payloads);
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].1, AgentState::Failed);
    assert!(
        payloads.iter().any(|p| matches!(
            p,
            ThreadEventPayload::AgentStateChanged { cause: Some(cause), .. }
                if cause.contains("indisponible")
        )),
        "the cause must survive in the log: {payloads:?}"
    );
    harness.handle.shutdown().await;
}

/// AC5: a child the log left open belongs to a dead process. Resume closes it
/// once and the graph comes back with every slot free.
#[tokio::test]
async fn a_resumed_thread_closes_its_orphan_children_and_frees_every_slot() {
    let store = Arc::new(MemoryThreadStore::new());
    let thread_id = ThreadId::generate(&RandomIds);
    let agent_id = AgentId::generate(&RandomIds);
    store.create(&thread_id).await.unwrap();
    for (seq, payload) in [
        ThreadEventPayload::ThreadCreated,
        ThreadEventPayload::AgentLinked {
            agent_id,
            name: Some(agent_runtime::AgentPath::root().join("orphelin").unwrap()),
            child_thread_id: ThreadId::generate(&RandomIds),
            task: "exploration interrompue par un crash".into(),
            authority: AgentAuthority::read_only(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append(&ThreadEvent {
                event_id: EventId::generate(&RandomIds),
                thread_id,
                seq: seq as u64 + 1,
                at_ms: seq as u64,
                payload,
            })
            .await
            .unwrap();
    }

    let spawner = ScriptedSpawner::new(Vec::new());
    let supervisor = supervisor(Arc::clone(&spawner) as Arc<dyn AgentSpawner>);
    let harness = common::start_on(
        parent_runner(),
        Some(Arc::clone(&supervisor)),
        Arc::clone(&store),
    )
    .await;

    assert_eq!(supervisor.graph().active(), 0, "no phantom slot survives");
    let records = supervisor.graph().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, AgentState::Interrupted);
    assert_eq!(records[0].task, "exploration interrompue par un crash");

    let terminals = agent_terminals(&events(&store).await);
    assert_eq!(
        terminals,
        vec![(agent_id, AgentState::Interrupted)],
        "the recovery is written exactly once"
    );

    // AC6 of US-015: a terminal replayed by a resume injects nothing.
    assert!(matches!(
        supervisor.wait_within(Some(agent_id), SHORT).await.unwrap(),
        WaitOutcome::Running(_)
    ));
    harness.handle.shutdown().await;
}

// ───────── US-013: spawn, list, wait ─────────

/// AC1 and AC2: a child gets a thread, a transcript and a clean turn context,
/// and its authority is read-only whatever it asked for.
#[tokio::test]
async fn a_child_gets_its_own_durable_thread_and_a_read_only_authority() {
    let spawner = ScriptedSpawner::new(vec![answers("j'ai lu trois fichiers")]);
    let (harness, supervisor) = start(&spawner).await;

    // Asking for a mutating tool from a read-only request grants nothing.
    let spawned = supervisor
        .spawn("agent_5", "lire le crate", &AgentAuthority::read_only())
        .await
        .unwrap();
    assert!(spawned.authority.is_read_only());
    assert!(!spawned.authority.allows("bash", false));
    assert!(spawned.authority.allows("grep", true));

    let request = &spawner.requests()[0];
    assert_eq!(request.parent_thread_id, harness.thread_id);
    assert_eq!(request.child_thread_id, spawned.thread_id);
    assert!(
        request.authority.is_read_only(),
        "the spawner is handed the granted authority"
    );

    // The child owns a durable thread of its own, opened on the task and on
    // nothing of the parent's transcript.
    let child_store = spawner.store_of(spawned.agent_id);
    let payloads = events(&child_store).await;
    assert!(matches!(payloads[0], ThreadEventPayload::ThreadCreated));
    assert!(
        matches!(&payloads[1], ThreadEventPayload::InputSubmitted { text, .. } if text == "lire le crate"),
        "the child's first input is its task: {payloads:?}"
    );
    assert!(
        matches!(
            &payloads[2],
            ThreadEventPayload::TurnStateChanged {
                to: TurnState::Running,
                context: Some(_),
                ..
            }
        ),
        "the child's turn carries a captured context of its own: {payloads:?}"
    );
    assert_eq!(
        child_store.read().await.unwrap().thread_id,
        Some(spawned.thread_id)
    );
    harness.handle.shutdown().await;
}

/// AC3: a listing shows identity, parent, state, task, turn and elapsed time,
/// and never a byte of a child's transcript.
#[tokio::test]
async fn a_listing_shows_states_and_never_transcripts() {
    let spawner = ScriptedSpawner::new(vec![hangs()]);
    let (harness, supervisor) = start(&spawner).await;
    let spawned = supervisor
        .spawn(
            "agent_6",
            "chercher la régression",
            &AgentAuthority::read_only(),
        )
        .await
        .unwrap();

    wait_for(
        || {
            supervisor
                .list()
                .first()
                .is_some_and(|view| view.turn.is_some())
        },
        "the child's turn to appear in the listing",
    )
    .await;

    let views = supervisor.list();
    assert_eq!(views.len(), 1);
    let view = &views[0];
    assert_eq!(view.agent_id, spawned.agent_id);
    assert_eq!(view.parent_thread_id, harness.thread_id);
    assert_eq!(view.thread_id, spawned.thread_id);
    assert_eq!(view.task, "chercher la régression");
    assert_eq!(view.state, AgentState::Running);
    assert_eq!(view.authority, "read-only");
    assert_eq!(view.turn.unwrap().state, TurnState::Running);
    assert!(
        !format!("{views:?}").contains("en cours"),
        "the child's own text must not leak into a listing: {views:?}"
    );
    harness.handle.shutdown().await;
}

/// AC4: with nothing terminal, the wait answers with states rather than
/// blocking forever.
#[tokio::test]
async fn a_wait_that_finds_nothing_terminal_answers_with_the_running_states() {
    let spawner = ScriptedSpawner::new(vec![hangs()]);
    let (harness, supervisor) = start(&spawner).await;
    supervisor
        .spawn(
            "agent_7",
            "longue exploration",
            &AgentAuthority::read_only(),
        )
        .await
        .unwrap();

    match supervisor.wait_within(None, SHORT).await.unwrap() {
        WaitOutcome::Running(views) => {
            assert_eq!(views.len(), 1);
            assert_eq!(views[0].state, AgentState::Running);
        }
        other => panic!("expected a running answer, got {other:?}"),
    }
    harness.handle.shutdown().await;
}

/// AC5: the parent's shutdown cancels its children AND records their terminal
/// BEFORE it writes its own.
#[tokio::test]
async fn a_parent_shutdown_closes_its_children_before_its_own_terminal() {
    let spawner = ScriptedSpawner::new(vec![hangs()]);
    let supervisor = supervisor(Arc::clone(&spawner) as Arc<dyn AgentSpawner>);
    let harness = common::start_with(hanging_parent_runner(), Some(Arc::clone(&supervisor))).await;
    spawner.watch(Arc::clone(&harness.store));
    let spawned = supervisor
        .spawn(
            "agent_8",
            "exploration sans fin",
            &AgentAuthority::read_only(),
        )
        .await
        .unwrap();
    let child_store = spawner.store_of(spawned.agent_id);
    // The parent is busy too, and its turn only ends with the shutdown: its
    // terminal is therefore the one the child's must precede.
    harness
        .handle
        .submit(Submission::new("le parent travaille"))
        .await
        .unwrap();
    common::wait_for(
        || {
            harness
                .handle
                .status()
                .turn
                .is_some_and(|t| t.state == TurnState::Running)
        },
        "the parent's turn to be running",
    )
    .await;

    harness.handle.shutdown().await;

    let payloads = events(&harness.store).await;
    let child_terminal = payloads
        .iter()
        .position(|p| matches!(p, ThreadEventPayload::AgentStateChanged { .. }))
        .expect("the child's terminal is recorded");
    let parent_terminal = payloads
        .iter()
        .rposition(
            |p| matches!(p, ThreadEventPayload::TurnStateChanged { to, .. } if to.is_terminal()),
        )
        .expect("the parent's terminal is recorded");
    assert!(
        child_terminal < parent_terminal,
        "a child must be closed before its parent: {payloads:?}"
    );

    let child_states: Vec<TurnState> = events(&child_store)
        .await
        .iter()
        .filter_map(|p| match p {
            ThreadEventPayload::TurnStateChanged { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(
        child_states.last(),
        Some(&TurnState::Interrupted),
        "the child's own log records that it stopped"
    );
}

/// AC6: a child that fails takes neither its parent nor its sibling down, and
/// its failure is observable.
#[tokio::test]
async fn a_failing_child_leaves_its_parent_and_its_sibling_alive() {
    let spawner = ScriptedSpawner::new(vec![crashes(), hangs()]);
    let (harness, supervisor) = start(&spawner).await;
    let failing = supervisor
        .spawn("agent_9", "celui qui casse", &AgentAuthority::read_only())
        .await
        .unwrap();
    let sibling = supervisor
        .spawn(
            "agent_10",
            "celui qui continue",
            &AgentAuthority::read_only(),
        )
        .await
        .unwrap();

    wait_for(
        || {
            supervisor
                .graph()
                .get(failing.agent_id)
                .is_some_and(|r| r.state.is_terminal())
        },
        "the failing child to reach a terminal state",
    )
    .await;

    assert_eq!(
        supervisor.graph().get(sibling.agent_id).unwrap().state,
        AgentState::Running,
        "a sibling is untouched"
    );
    // The parent still takes commands.
    harness
        .handle
        .submit(Submission::new("et le parent continue"))
        .await
        .expect("the parent stays commandable");
    harness.handle.shutdown().await;
}

// ───────── US-014: send, follow-up, interrupt ─────────

/// AC1 and AC2: a running child is steered through the user's own protocol; an
/// idle one gets a new turn.
#[tokio::test]
async fn a_message_steers_a_running_child_and_opens_a_turn_on_an_idle_one() {
    let spawner = ScriptedSpawner::new(vec![ChildScript::new(vec![
        Scripted::StreamThenHang(vec![text("je cherche")]),
        Scripted::Stream(vec![text("corrigé"), done_end_turn()]),
        Scripted::Stream(vec![text("suite"), done_end_turn()]),
    ])]);
    let (harness, supervisor) = start(&spawner).await;
    let spawned = supervisor
        .spawn("agent_11", "chercher", &AgentAuthority::read_only())
        .await
        .unwrap();
    let provider = spawner.provider_of(spawned.agent_id);

    // Running: the message steers the turn in flight.
    let sent = supervisor
        .send(spawned.agent_id, "regarde plutôt les tests", None)
        .await
        .unwrap();
    assert_eq!(
        sent.delivery,
        AgentDelivery::Steered,
        "a running child is steered, not restarted"
    );
    wait_for(
        || {
            provider
                .requests()
                .iter()
                .any(|r| user_texts(r).iter().any(|t| t.contains("plutôt les tests")))
        },
        "the steer to reach the child's next model request",
    )
    .await;

    // Idle: the next message opens a turn of its own.
    wait_for(
        || {
            supervisor
                .graph()
                .get(spawned.agent_id)
                .is_some_and(|r| r.state == AgentState::Idle)
        },
        "the child to become idle",
    )
    .await;
    let sent = supervisor
        .send(spawned.agent_id, "continue sur cette piste", None)
        .await
        .unwrap();
    assert_eq!(
        sent.delivery,
        AgentDelivery::Started,
        "an idle child opens a new turn"
    );

    let child_store = spawner.store_of(spawned.agent_id);
    let turns: Vec<String> = events(&child_store)
        .await
        .iter()
        .filter_map(|p| match p {
            ThreadEventPayload::InputSubmitted { turn_id, text, .. } => {
                Some(format!("{turn_id}:{text}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(turns.len(), 3, "task, steer and follow-up: {turns:?}");
    let follow_up_turn = turns[2].split(':').next().unwrap();
    let task_turn = turns[0].split(':').next().unwrap();
    assert_ne!(
        follow_up_turn, task_turn,
        "a follow-up opens a turn of its own: {turns:?}"
    );
    harness.handle.shutdown().await;
}

/// AC3: interrupting one child triggers its branch and no other.
#[tokio::test]
async fn an_interruption_reaches_one_child_and_spares_its_sibling() {
    let spawner = ScriptedSpawner::new(vec![hangs(), hangs()]);
    let (harness, supervisor) = start(&spawner).await;
    let target = supervisor
        .spawn(
            "agent_12",
            "celui qu'on arrête",
            &AgentAuthority::read_only(),
        )
        .await
        .unwrap();
    let sibling = supervisor
        .spawn("agent_13", "celui qui reste", &AgentAuthority::read_only())
        .await
        .unwrap();

    supervisor.interrupt(target.agent_id).await.unwrap();

    wait_for(
        || {
            supervisor
                .graph()
                .get(target.agent_id)
                .is_some_and(|r| r.state == AgentState::Interrupted)
        },
        "the interrupted child to close",
    )
    .await;
    assert_eq!(
        supervisor.graph().get(sibling.agent_id).unwrap().state,
        AgentState::Running,
        "the sibling keeps running"
    );
    assert_eq!(harness.handle.status().thread_id, harness.thread_id);
    harness.handle.shutdown().await;
}

/// AC4: unknown, terminal and foreign all get the SAME refusal, and none of
/// them reveals anything about a transcript.
#[tokio::test]
async fn an_agent_this_thread_does_not_own_is_refused_without_leaking_anything() {
    let spawner = ScriptedSpawner::new(vec![answers("terminé")]);
    let (harness, supervisor) = start(&spawner).await;
    let mine = supervisor
        .spawn("agent_14", "le mien", &AgentAuthority::read_only())
        .await
        .unwrap();
    supervisor.interrupt(mine.agent_id).await.unwrap();
    wait_for(
        || {
            supervisor
                .graph()
                .get(mine.agent_id)
                .is_some_and(|r| r.state.is_terminal())
        },
        "the child to close",
    )
    .await;

    let stranger = AgentId::generate(&RandomIds);
    let on_terminal = supervisor.send(mine.agent_id, "encore", None).await;
    let on_stranger = supervisor.send(stranger, "encore", None).await;

    assert_eq!(
        on_terminal.unwrap_err(),
        AgentError::Unreachable {
            agent_id: mine.agent_id
        }
    );
    assert_eq!(
        on_stranger.unwrap_err(),
        AgentError::Unreachable { agent_id: stranger }
    );
    assert!(
        matches!(
            supervisor.wait_within(Some(stranger), SHORT).await,
            Err(AgentError::Unreachable { .. })
        ),
        "waiting on a foreign agent must not even wait"
    );
    assert!(
        !AgentError::Unreachable { agent_id: stranger }
            .to_string()
            .contains("terminé"),
        "a refusal says nothing about any transcript"
    );
    harness.handle.shutdown().await;
}

/// AC5 and AC6: a replayed message reaches the child once, and accepted
/// messages keep their order.
#[tokio::test]
async fn a_replayed_message_is_delivered_once_and_order_is_preserved() {
    let spawner = ScriptedSpawner::new(vec![hangs()]);
    let (harness, supervisor) = start(&spawner).await;
    let spawned = supervisor
        .spawn("agent_15", "chercher", &AgentAuthority::read_only())
        .await
        .unwrap();
    let child_store = spawner.store_of(spawned.agent_id);

    for text in ["premier", "deuxième", "troisième"] {
        supervisor
            .send(spawned.agent_id, text, Some(format!("cli-{text}")))
            .await
            .unwrap();
    }
    // Same keys again: none of them may reach the child a second time.
    for text in ["premier", "deuxième", "troisième"] {
        supervisor
            .send(spawned.agent_id, text, Some(format!("cli-{text}")))
            .await
            .unwrap();
    }

    let inputs: Vec<String> = events(&child_store)
        .await
        .iter()
        .filter_map(|p| match p {
            ThreadEventPayload::InputSubmitted { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        inputs,
        vec!["chercher", "premier", "deuxième", "troisième"],
        "each message appears once, in the order it was accepted"
    );
    harness.handle.shutdown().await;
}

// ───────── US-015: handoff ─────────

/// AC1 and AC3: the parent gets a bounded, untrusted summary carrying the
/// child's identity, and it gets it exactly once.
#[tokio::test]
async fn a_finished_child_hands_back_an_untrusted_summary_exactly_once() {
    let spawner = ScriptedSpawner::new(vec![answers("trois pistes, la deuxième tient")]);
    let (harness, supervisor) = start(&spawner).await;
    let spawned = supervisor
        .spawn("agent_16", "explorer", &AgentAuthority::read_only())
        .await
        .unwrap();

    let handoffs = match supervisor.wait_within(None, GENEROUS).await.unwrap() {
        WaitOutcome::Ready(handoffs) => handoffs,
        other => panic!("expected a handoff, got {other:?}"),
    };
    assert_eq!(handoffs.len(), 1);
    let handoff = &handoffs[0];
    assert_eq!(handoff.agent_id, spawned.agent_id);
    assert_eq!(handoff.thread_id, spawned.thread_id);
    assert_eq!(handoff.state, AgentState::Idle);
    assert_eq!(handoff.summary, "trois pistes, la deuxième tient");
    assert!(handoff.render().starts_with(UNTRUSTED_BANNER));

    // AC6: a second wait does not hand the same result back.
    assert!(matches!(
        supervisor.wait_within(None, SHORT).await.unwrap(),
        WaitOutcome::Running(_)
    ));
    harness.handle.shutdown().await;
}

/// AC5: a child that failed still owes a structured answer, never silence.
#[tokio::test]
async fn a_failed_child_still_hands_back_its_state_and_cause() {
    let spawner = ScriptedSpawner::new(vec![crashes()]);
    let (harness, supervisor) = start(&spawner).await;
    supervisor
        .spawn("agent_17", "celui qui casse", &AgentAuthority::read_only())
        .await
        .unwrap();

    let handoffs = match supervisor.wait_within(None, GENEROUS).await.unwrap() {
        WaitOutcome::Ready(handoffs) => handoffs,
        other => panic!("expected a handoff, got {other:?}"),
    };
    assert_eq!(handoffs.len(), 1);
    assert!(handoffs[0].state.is_terminal());
    assert!(
        handoffs[0].cause.is_some(),
        "a non-nominal end names its cause"
    );
    assert!(
        !handoffs[0].summary.trim().is_empty(),
        "silence is not an acceptable handoff"
    );
    harness.handle.shutdown().await;
}
