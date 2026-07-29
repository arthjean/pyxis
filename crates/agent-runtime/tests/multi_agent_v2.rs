//! Acceptance of EP-003: multi-agent v2, durable and interruptible.
//!
//! What EP-004 proved was the graph and the leases. What this suite proves is
//! the v2 contract on top of them: canonical names, `send_message` versus
//! `followup_task`, a graph that survives a restart with its undelivered mail,
//! a child whose own log is gone, and a cascade that leaves no descendant
//! without a persisted cause.
//!
//! Every test drives the production seam: a real `ThreadHandle` for the parent,
//! a real `AgentSupervisor`, a real `RunAgentRunner` per child. The spawner is
//! the one piece a binary owns, so it is a fake here, but a DURABLE one: it
//! hands the same store back for the same thread, which is what a restart
//! actually looks like.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_runtime::agent::{AgentAuthority, AgentError, AgentState};
use agent_runtime::event::ThreadEventPayload;
use agent_runtime::id::{RandomIds, ThreadId};
use agent_runtime::path::AgentPath;
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::supervisor::{
    AgentDelivery, AgentSpawner, AgentSupervisor, ChildParts, ChildRequest,
};
use common::{
    FakeProvider, FakeSession, Harness, InstantClock, Scripted, agent_context, deps, done_end_turn,
    text, user_texts, wait_for,
};

const SHORT: Duration = Duration::from_millis(150);

/// Builds children on stores keyed by THREAD, so reopening the same child gives
/// it back its own durable log. That is the property a restart depends on.
struct DurableSpawner {
    scripts: Mutex<Vec<Vec<Scripted>>>,
    stores: Mutex<HashMap<ThreadId, Arc<MemoryThreadStore>>>,
    providers: Mutex<HashMap<ThreadId, Arc<FakeProvider>>>,
    requests: Mutex<Vec<ChildRequest>>,
    /// Refuses every spawn: a binary whose child runtime is unavailable.
    broken: AtomicBool,
    /// Refuses only a REOPEN: a child whose own log is corrupt or missing.
    broken_on_resume: AtomicBool,
}

impl DurableSpawner {
    fn new(scripts: Vec<Vec<Scripted>>) -> Arc<Self> {
        scripts.iter().for_each(drop);
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().rev().collect()),
            stores: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            broken: AtomicBool::new(false),
            broken_on_resume: AtomicBool::new(false),
        })
    }

    fn break_all(&self) {
        self.broken.store(true, Ordering::SeqCst);
    }

    fn break_resume(&self) {
        self.broken_on_resume.store(true, Ordering::SeqCst);
    }

    fn requests(&self) -> Vec<ChildRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn store_of(&self, thread_id: ThreadId) -> Arc<MemoryThreadStore> {
        Arc::clone(
            self.stores
                .lock()
                .unwrap()
                .get(&thread_id)
                .expect("a store for this child"),
        )
    }

    fn provider_of(&self, thread_id: ThreadId) -> Arc<FakeProvider> {
        Arc::clone(
            self.providers
                .lock()
                .unwrap()
                .get(&thread_id)
                .expect("a provider for this child"),
        )
    }
}

#[async_trait::async_trait]
impl AgentSpawner for DurableSpawner {
    async fn spawn(&self, request: &ChildRequest) -> Result<ChildParts, String> {
        if self.broken.load(Ordering::SeqCst) {
            return Err("child runtime unavailable".into());
        }
        if request.resumed && self.broken_on_resume.load(Ordering::SeqCst) {
            return Err("child log is unreadable".into());
        }
        self.requests.lock().unwrap().push(request.clone());

        let store = Arc::clone(
            self.stores
                .lock()
                .unwrap()
                .entry(request.child_thread_id)
                .or_insert_with(|| Arc::new(MemoryThreadStore::new())),
        );
        // A reopened child gets its own log back, exactly as a file adapter
        // would: same path, same content, admission open again.
        store.reopen();
        // One provider per CHILD, re-armed on a reopen with whatever the test
        // scripted next: a reopened child answers its follow-up.
        let turns = self.scripts.lock().unwrap().pop().unwrap_or_default();
        let provider = FakeProvider::new(turns);
        self.providers
            .lock()
            .unwrap()
            .insert(request.child_thread_id, Arc::clone(&provider));

        Ok(ChildParts {
            store: store as Arc<dyn ThreadStore>,
            runner: Arc::new(agent_runtime::runner::RunAgentRunner::new(
                deps(provider, FakeSession::new(), Arc::new(common::EchoTools)),
                agent_context,
            )),
            turn_contexts: Arc::new(agent_runtime::context::FixedTurnContext::new(
                common::turn_context(agent_runtime::id::TurnId::generate(&RandomIds)),
            )),
        })
    }
}

fn answers(reply: &str) -> Vec<Scripted> {
    vec![Scripted::Stream(vec![text(reply), done_end_turn()])]
}

fn hangs() -> Vec<Scripted> {
    vec![Scripted::StreamThenHang(vec![text("en cours")])]
}

fn supervisor(spawner: &Arc<DurableSpawner>) -> Arc<AgentSupervisor> {
    AgentSupervisor::new(
        Arc::clone(spawner) as Arc<dyn AgentSpawner>,
        Arc::new(RandomIds),
        Arc::new(InstantClock),
        AgentAuthority::unrestricted(),
    )
}

/// A parent that answers instantly: this suite is about its children.
fn parent_runner() -> Arc<dyn agent_runtime::runner::TurnRunner> {
    Arc::new(agent_runtime::runner::RunAgentRunner::new(
        deps(
            FakeProvider::new(Vec::new()),
            FakeSession::new(),
            Arc::new(common::EchoTools),
        ),
        agent_context,
    ))
}

/// Opens a parent thread on `store` with a fresh supervisor over `spawner`.
async fn open(
    spawner: &Arc<DurableSpawner>,
    store: Arc<MemoryThreadStore>,
) -> (Harness, Arc<AgentSupervisor>) {
    // A process that went away closed its store; the next one opens the same
    // durable log.
    store.reopen();
    let supervisor = supervisor(spawner);
    let harness = common::start_on(parent_runner(), Some(Arc::clone(&supervisor)), store).await;
    (harness, supervisor)
}

async fn payloads(store: &Arc<MemoryThreadStore>) -> Vec<ThreadEventPayload> {
    store
        .read()
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

async fn wait_idle(supervisor: &Arc<AgentSupervisor>, name: &str) {
    wait_for(
        || {
            supervisor
                .resolve(name)
                .is_ok_and(|record| record.state == AgentState::Idle)
        },
        "the child to become idle",
    )
    .await;
}

// ───────── US-011: the v2 surface in the binary ─────────

/// AC2: a spawn carries its canonical name into the graph, into the parent's
/// durable log and into what the spawner is handed, together with the
/// intersected authority. A name is the handle everything else uses, so it has
/// to be the same one in all three places.
#[tokio::test]
async fn a_spawn_persists_its_canonical_name_filiation_and_intersected_authority() {
    let spawner = DurableSpawner::new(vec![answers("fait")]);
    let (harness, supervisor) = open(&spawner, Arc::new(MemoryThreadStore::new())).await;

    let spawned = supervisor
        .spawn("reader", "lire le crate", &AgentAuthority::read_only())
        .await
        .expect("the spawn is accepted");

    assert_eq!(spawned.name, AgentPath::root().join("reader").unwrap());
    assert!(spawned.authority.is_read_only());

    let linked = payloads(&harness.store)
        .await
        .into_iter()
        .find_map(|payload| match payload {
            ThreadEventPayload::AgentLinked {
                name, authority, ..
            } => Some((name, authority)),
            _ => None,
        })
        .expect("the filiation is durable");
    assert_eq!(linked.0, Some(spawned.name.clone()));
    assert!(linked.1.is_read_only());

    let request = spawner.requests().pop().expect("the spawner saw the child");
    assert_eq!(request.name, spawned.name);
    assert!(!request.resumed, "a first creation is not a reopening");

    // The same name addresses the child, relatively and canonically.
    assert_eq!(
        supervisor.resolve("reader").unwrap().agent_id,
        spawned.agent_id
    );
    assert_eq!(
        supervisor.resolve("/root/reader").unwrap().agent_id,
        spawned.agent_id
    );

    // US-013 AC4: the listing names the OWNER, which is the thread and never
    // the call that spawned the child. A caller going away leaves the child
    // exactly where this says it is.
    let view = supervisor
        .list()
        .into_iter()
        .find(|view| view.agent_id == spawned.agent_id)
        .expect("the child is listed");
    assert_eq!(view.name, spawned.name);
    assert_eq!(view.parent_thread_id, harness.thread_id);
    assert_eq!(view.thread_id, spawned.thread_id);
    assert!(view.attached, "this process holds the child's thread");
    assert_eq!(view.pending_messages, 0);
    harness.handle.shutdown().await;
}

/// AC3: when the child runtime is unavailable, the call fails with a typed
/// error and the graph is left with a FAILED node carrying its cause, not with
/// an orphan the parent would keep waiting on.
#[tokio::test]
async fn an_unavailable_child_runtime_fails_typed_and_leaves_no_orphan() {
    let spawner = DurableSpawner::new(Vec::new());
    spawner.break_all();
    let (harness, supervisor) = open(&spawner, Arc::new(MemoryThreadStore::new())).await;

    let error = supervisor
        .spawn("reader", "lire le crate", &AgentAuthority::read_only())
        .await
        .expect_err("an unavailable runtime must refuse");
    assert!(matches!(error, AgentError::Spawn(_)), "{error:?}");

    let record = supervisor.resolve("reader").expect("the node is accounted");
    assert_eq!(record.state, AgentState::Failed);
    assert!(record.cause.is_some(), "a failure keeps its cause");
    assert_eq!(supervisor.graph().active(), 0, "no slot is left held");

    let states: Vec<AgentState> = payloads(&harness.store)
        .await
        .into_iter()
        .filter_map(|payload| match payload {
            ThreadEventPayload::AgentStateChanged { to, .. } => Some(to),
            _ => None,
        })
        .collect();
    assert_eq!(states, vec![AgentState::Failed]);
    harness.handle.shutdown().await;
}

// ───────── US-012: durability and interruption ─────────

/// AC1: a restart rebuilds the graph, the canonical names and the mail nobody
/// took, and closes what was running with a cause. AC2: the follow-up that
/// reopens the child hands it the queued message and the new task, once and in
/// order.
#[tokio::test]
async fn a_restart_rebuilds_names_states_and_undelivered_mail() {
    let spawner = DurableSpawner::new(vec![answers("premier"), answers("second")]);
    let store = Arc::new(MemoryThreadStore::new());
    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;

    let spawned = supervisor
        .spawn("reader", "lire le crate", &AgentAuthority::read_only())
        .await
        .unwrap();
    wait_idle(&supervisor, "reader").await;

    // `send_message` opens no turn: it waits durably.
    let sent = supervisor
        .send_message(spawned.agent_id, "note pour plus tard", None)
        .await
        .unwrap();
    assert_eq!(sent.delivery, AgentDelivery::Queued);
    assert!(
        payloads(&harness.store)
            .await
            .iter()
            .any(|payload| matches!(
                payload,
                ThreadEventPayload::AgentMessageQueued { text, .. } if text == "note pour plus tard"
            )),
        "a queued message is durable before it is acknowledged"
    );
    harness.handle.shutdown().await;

    // ── restart on the same log, with a fresh supervisor ──
    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    let restored = supervisor
        .resolve("reader")
        .expect("the canonical name survives the restart");
    assert_eq!(restored.agent_id, spawned.agent_id);
    assert_eq!(
        restored.state,
        AgentState::Idle,
        "an idle child stays addressable across a restart"
    );
    let view = supervisor
        .list()
        .into_iter()
        .find(|view| view.name == restored.name)
        .unwrap();
    assert_eq!(view.pending_messages, 1, "the mail survives too");
    assert!(
        !view.attached,
        "no actor is held before something addresses it"
    );

    // AC2: the follow-up reopens the child's OWN log and carries the queued
    // message with it.
    let sent = supervisor
        .followup_task(restored.agent_id, "continue sur cette piste", None)
        .await
        .unwrap();
    assert_eq!(sent.delivery, AgentDelivery::Started);

    let child_provider = spawner.provider_of(restored.thread_id);
    wait_for(
        || {
            child_provider.requests().iter().any(|request| {
                user_texts(request)
                    .iter()
                    .any(|text| text.contains("note pour plus tard") && text.contains("continue"))
            })
        },
        "the queued message and the follow-up to reach the child in one turn",
    )
    .await;

    // Consumed exactly once: the log says so, and a second restart must not
    // replay it.
    let after = payloads(&harness.store).await;
    assert_eq!(
        after
            .iter()
            .filter(|payload| matches!(payload, ThreadEventPayload::AgentMessageDelivered { .. }))
            .count(),
        1
    );
    let reopened = spawner
        .requests()
        .into_iter()
        .filter(|request| request.resumed)
        .count();
    assert_eq!(reopened, 1, "the child is reopened once, not respawned");
    // Its log is its own: the reopened child appended to the file that already
    // carried its first turn, instead of starting a second one.
    let child_inputs: Vec<String> = payloads(&spawner.store_of(restored.thread_id))
        .await
        .into_iter()
        .filter_map(|payload| match payload {
            ThreadEventPayload::InputSubmitted { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(child_inputs.len(), 2, "{child_inputs:?}");
    assert!(child_inputs[0].contains("lire le crate"));
    assert!(child_inputs[1].contains("continue sur cette piste"));
    harness.handle.shutdown().await;
}

/// AC1, the trap: reopening a child must not hand its parent a SECOND result
/// for a turn that already ended in another process. A restart brings back the
/// child's last terminal, and a watcher starting from scratch would read it as
/// news.
#[tokio::test]
async fn reopening_a_child_does_not_replay_the_handoff_it_already_gave() {
    let spawner = DurableSpawner::new(vec![answers("resultat"), answers("suite")]);
    let store = Arc::new(MemoryThreadStore::new());
    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    let spawned = supervisor
        .spawn("reader", "lire", &AgentAuthority::read_only())
        .await
        .unwrap();
    wait_idle(&supervisor, "reader").await;

    // The parent collects the first result, once.
    let first = supervisor
        .wait_within(Some(spawned.agent_id), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(
        first,
        agent_runtime::supervisor::WaitOutcome::Ready(_)
    ));
    harness.handle.shutdown().await;

    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    let restored = supervisor.resolve("reader").unwrap();
    // Reopening alone, through a message that opens no turn.
    supervisor
        .send_message(restored.agent_id, "note", None)
        .await
        .unwrap();
    assert!(
        matches!(
            supervisor
                .wait_within(Some(restored.agent_id), SHORT)
                .await
                .unwrap(),
            agent_runtime::supervisor::WaitOutcome::Running(_)
        ),
        "a reopened child owes nothing until it runs again"
    );
    harness.handle.shutdown().await;
}

/// AC2, running side: a follow-up to a RUNNING child is delivered into the turn
/// in flight. Never a second concurrent turn, which is the one thing a parent
/// must not be able to cause.
#[tokio::test]
async fn a_followup_on_a_running_child_never_opens_a_second_turn() {
    let spawner = DurableSpawner::new(vec![hangs()]);
    let (harness, supervisor) = open(&spawner, Arc::new(MemoryThreadStore::new())).await;
    let spawned = supervisor
        .spawn("reader", "exploration longue", &AgentAuthority::read_only())
        .await
        .unwrap();

    let sent = supervisor
        .followup_task(spawned.agent_id, "regarde plutôt les tests", None)
        .await
        .unwrap();
    assert_eq!(sent.delivery, AgentDelivery::Steered);

    let child = spawner.store_of(spawned.thread_id);
    let submitted = |payloads: Vec<ThreadEventPayload>| {
        payloads
            .into_iter()
            .filter_map(|payload| match payload {
                ThreadEventPayload::InputSubmitted { turn_id, text, .. } => Some((turn_id, text)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    wait_for(
        || {
            futures_util::future::FutureExt::now_or_never(payloads(&child))
                .map(&submitted)
                .is_some_and(|inputs| inputs.len() == 2)
        },
        "the task and the follow-up to be durable on the child",
    )
    .await;
    let inputs = submitted(payloads(&child).await);
    assert_eq!(
        inputs[0].0, inputs[1].0,
        "the follow-up joined the running turn instead of opening one: {inputs:?}"
    );
    harness.handle.shutdown().await;
}

/// AC4: a child whose own log cannot be reopened becomes `failed` with a
/// visible cause, and its siblings stay drivable. One broken descendant must
/// not take the tree down with it.
#[tokio::test]
async fn a_child_whose_log_is_unreadable_fails_alone() {
    let spawner = DurableSpawner::new(vec![answers("un"), answers("deux"), answers("trois")]);
    let store = Arc::new(MemoryThreadStore::new());
    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    supervisor
        .spawn("reader", "lire", &AgentAuthority::read_only())
        .await
        .unwrap();
    supervisor
        .spawn("writer", "ecrire", &AgentAuthority::read_only())
        .await
        .unwrap();
    wait_idle(&supervisor, "reader").await;
    wait_idle(&supervisor, "writer").await;
    harness.handle.shutdown().await;

    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    spawner.break_resume();
    let broken = supervisor.resolve("reader").unwrap();
    let error = supervisor
        .followup_task(broken.agent_id, "continue", None)
        .await
        .expect_err("an unreadable child log must refuse");
    assert!(matches!(error, AgentError::Spawn(_)), "{error:?}");

    let failed = supervisor.resolve("reader").unwrap();
    assert_eq!(failed.state, AgentState::Failed);
    assert!(
        failed
            .cause
            .as_deref()
            .is_some_and(|cause| cause.contains("unreadable")),
        "the cause names what went wrong: {:?}",
        failed.cause
    );
    assert!(
        payloads(&harness.store)
            .await
            .iter()
            .any(|payload| matches!(
                payload,
                ThreadEventPayload::AgentStateChanged {
                    to: AgentState::Failed,
                    ..
                }
            )),
        "the failure is durable"
    );

    // The sibling is untouched and still reachable.
    let sibling = supervisor.resolve("writer").unwrap();
    assert_eq!(sibling.state, AgentState::Idle);
    harness.handle.shutdown().await;
}

/// AC3: a parent shutdown reaches every ACTIVE descendant, and each one leaves
/// a persisted cause. An idle child is deliberately untouched: it holds no
/// process, and closing it would destroy exactly what a restart looks for.
#[tokio::test]
async fn a_parent_shutdown_persists_a_cause_for_every_active_descendant() {
    let spawner = DurableSpawner::new(vec![hangs(), hangs(), answers("fini")]);
    let store = Arc::new(MemoryThreadStore::new());
    let (harness, supervisor) = open(&spawner, Arc::clone(&store)).await;
    for (name, task) in [
        ("first", "sans fin 1"),
        ("second", "sans fin 2"),
        ("third", "court"),
    ] {
        supervisor
            .spawn(name, task, &AgentAuthority::read_only())
            .await
            .unwrap();
    }
    wait_idle(&supervisor, "third").await;

    harness.handle.shutdown().await;

    let after = payloads(&store).await;
    let closed: Vec<AgentState> = after
        .iter()
        .filter_map(|payload| match payload {
            ThreadEventPayload::AgentStateChanged { to, cause, .. } if to.is_terminal() => {
                assert!(cause.is_some(), "every terminal carries a cause");
                Some(*to)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        closed.len(),
        2,
        "the two running children are closed, the idle one is not: {closed:?}"
    );
    assert!(closed.iter().all(|state| state.is_terminal()));

    // And the log is causal: a filiation always precedes its own terminal.
    let order: Vec<&ThreadEventPayload> = after
        .iter()
        .filter(|payload| {
            matches!(
                payload,
                ThreadEventPayload::AgentLinked { .. }
                    | ThreadEventPayload::AgentStateChanged { .. }
            )
        })
        .collect();
    let mut linked = Vec::new();
    for payload in order {
        match payload {
            ThreadEventPayload::AgentLinked { agent_id, .. } => linked.push(*agent_id),
            ThreadEventPayload::AgentStateChanged { agent_id, .. } => {
                assert!(
                    linked.contains(agent_id),
                    "a state change precedes its own filiation"
                );
            }
            _ => {}
        }
    }
}

// ───────── US-013: one semantics, two call paths ─────────

/// AC3: a name already taken, a name that is not one, and an authority a parent
/// does not hold are all refused BEFORE a child exists. Whichever path the call
/// came from, nothing is created.
#[tokio::test]
async fn a_cycle_a_bad_name_and_an_escalation_are_refused_before_creation() {
    let spawner = DurableSpawner::new(vec![hangs(), hangs()]);
    let (harness, supervisor) = open(&spawner, Arc::new(MemoryThreadStore::new())).await;
    supervisor
        .spawn("reader", "lire", &AgentAuthority::read_only())
        .await
        .unwrap();
    let created_after_first = spawner.requests().len();

    assert!(matches!(
        supervisor
            .spawn("reader", "encore", &AgentAuthority::read_only())
            .await,
        Err(AgentError::NameTaken { .. })
    ));
    assert!(matches!(
        supervisor
            .spawn("Reader", "majuscule", &AgentAuthority::read_only())
            .await,
        Err(AgentError::InvalidName(_))
    ));
    assert!(matches!(
        supervisor
            .spawn("root", "reserve", &AgentAuthority::read_only())
            .await,
        Err(AgentError::InvalidName(_))
    ));
    assert_eq!(
        spawner.requests().len(),
        created_after_first,
        "a refused spawn never reaches the spawner"
    );
    assert_eq!(supervisor.graph().created(), 1, "and burns no creation");

    // An escalation does not fail: it is NARROWED, which is the only safe way
    // to answer a request for something the parent never held.
    let child = supervisor
        .spawn(
            "writer",
            "ecrire",
            &AgentAuthority::with_tools(["bash", "edit"]),
        )
        .await
        .unwrap();
    let granted = supervisor.resolve("writer").unwrap().authority;
    assert!(granted.allows("bash", false), "the root holds everything");
    assert_eq!(child.authority, granted);

    // From a READ-ONLY parent the same request grants nothing.
    let read_only = AgentSupervisor::new(
        Arc::clone(&spawner) as Arc<dyn AgentSpawner>,
        Arc::new(RandomIds),
        Arc::new(InstantClock),
        AgentAuthority::read_only(),
    );
    assert!(
        read_only
            .authority()
            .grant(&AgentAuthority::with_tools(["bash"]))
            .is_read_only()
    );
    harness.handle.shutdown().await;
}

/// AC2: two children answering at once wake exactly the wait that asked for
/// each of them. A handoff crosses to its own caller and to no other.
#[tokio::test]
async fn a_child_handoff_wakes_only_the_wait_that_asked_for_it() {
    let spawner = DurableSpawner::new(vec![answers("resultat reader"), hangs()]);
    let (harness, supervisor) = open(&spawner, Arc::new(MemoryThreadStore::new())).await;
    let reader = supervisor
        .spawn("reader", "lire", &AgentAuthority::read_only())
        .await
        .unwrap();
    let writer = supervisor
        .spawn("writer", "ecrire sans fin", &AgentAuthority::read_only())
        .await
        .unwrap();

    // The wait targeting the child that never finishes gets the states back,
    // not the other child's result.
    let (on_writer, on_reader) = tokio::join!(
        supervisor.wait_within(Some(writer.agent_id), SHORT),
        supervisor.wait_within(Some(reader.agent_id), Duration::from_secs(5)),
    );
    assert!(matches!(
        on_writer.unwrap(),
        agent_runtime::supervisor::WaitOutcome::Running(_)
    ));
    let handoffs = match on_reader.unwrap() {
        agent_runtime::supervisor::WaitOutcome::Ready(handoffs) => handoffs,
        other => panic!("the finished child owes a handoff: {other:?}"),
    };
    assert_eq!(handoffs.len(), 1);
    assert_eq!(handoffs[0].agent_id, reader.agent_id);

    // And exactly once: a second wait on the same child finds nothing new.
    assert!(matches!(
        supervisor
            .wait_within(Some(reader.agent_id), SHORT)
            .await
            .unwrap(),
        agent_runtime::supervisor::WaitOutcome::Running(_)
    ));
    harness.handle.shutdown().await;
}
