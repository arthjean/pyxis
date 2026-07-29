//! Bounded sub-agent graph: authority, leases and filiation (US-012).
//!
//! A child is PARENT-OWNED. It never widens what its parent could do, it never
//! reaches a sibling, and the parallelism it costs is bounded by three constants
//! rather than by a configuration key (FR-15, FR-20).
//!
//! The graph is the accounting side of that promise: it reserves a slot, an
//! identifier and an edge in ONE critical section, so two spawns racing for the
//! last slot cannot both believe they won. What it deliberately does not do is
//! create anything: the store, the engine and the thread of a child belong to
//! [`crate::supervisor`], which asks the graph for a place first and reports
//! back what happened to it.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::id::{AgentId, ThreadId};
use crate::path::{AgentPath, AgentPathError};

/// Children a thread may run AT ONCE. The bound is on concurrency, which is
/// what actually costs provider calls and context.
pub const MAX_ACTIVE_AGENTS: usize = 4;
/// Children a thread may ever create. Bounds a spawn loop even when every child
/// finishes instantly.
pub const MAX_AGENTS_PER_ROOT: usize = 8;
/// Depth of the agent tree. `1` means a child has no child of its own.
pub const MAX_AGENT_DEPTH: usize = 1;
/// How long a wait blocks before answering "still running" instead (US-013 AC4).
pub const AGENT_WAIT_WINDOW: Duration = Duration::from_secs(10);
/// Cause persisted for a child whose turn a restart interrupted. Named so every
/// surface reports the same sentence for the same event (US-019).
pub const RESTART_CAUSE: &str =
    "interrupted: the process restarted while the sub-agent was running";

/// Lifecycle of a sub-agent, from its parent's point of view.
///
/// `Idle` is the state that makes a follow-up possible (US-014 AC1): the child
/// finished a turn, published its handoff and released its slot, but its thread
/// is still there and can take another turn. `Interrupted` and `Failed` are
/// terminal: the thread is gone and no input can reach it anymore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// A turn is running. Occupies one of the [`MAX_ACTIVE_AGENTS`] slots.
    Running,
    /// Last turn completed; the child waits for a follow-up.
    Idle,
    /// Terminal: cancelled by its parent, by a shutdown or by a resume.
    Interrupted,
    /// Terminal: the child's provider, store or engine failed.
    Failed,
}

impl AgentState {
    /// Does this state hold one of the concurrency slots?
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }

    /// Terminal states accept no further input and free their thread.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Interrupted | Self::Failed)
    }

    /// Can the parent still address this child?
    pub fn accepts_input(self) -> bool {
        matches!(self, Self::Running | Self::Idle)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an agent is allowed to do (FR-16).
///
/// The only way to build a child's authority is [`AgentAuthority::grant`], and
/// that operation can only REMOVE: read-only is sticky downwards and a tool the
/// parent does not hold cannot be handed to a child. There is deliberately no
/// constructor that widens an existing authority, so "a child is never more
/// powerful than its parent" is a property of this type rather than of a code
/// path someone has to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuthority {
    /// The holder may only run tools that mutate nothing.
    read_only: bool,
    /// Mutating tools explicitly held. `None` means "every mutating tool", which
    /// is the authority of a user-driven thread and of nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    granted: Option<BTreeSet<String>>,
}

impl AgentAuthority {
    /// Authority of a user-driven thread: the permission pipeline of
    /// `agent-tools` is what bounds it, not this type.
    pub fn unrestricted() -> Self {
        Self {
            read_only: false,
            granted: None,
        }
    }

    /// Read-only authority: no mutating tool at all. The v1 default for a child
    /// (US-013 AC2) and the floor every `grant` converges to.
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            granted: Some(BTreeSet::new()),
        }
    }

    /// Authority holding exactly these mutating tools.
    pub fn with_tools<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            read_only: false,
            granted: Some(tools.into_iter().map(Into::into).collect()),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Mutating tools held, or `None` for "every one of them".
    pub fn granted_tools(&self) -> Option<&BTreeSet<String>> {
        self.granted.as_ref()
    }

    /// Authority of a child asking for `request` from this parent: the
    /// INTERSECTION of the two, never anything else.
    ///
    /// Read-only propagates downwards (a read-only parent can only produce
    /// read-only children) and the granted set is intersected, `None` acting as
    /// the universe on either side. A request naming a tool the parent does not
    /// hold silently loses it rather than failing: a child that quietly gets
    /// less is safe, a child that gets more is the risk this whole type exists
    /// to remove.
    pub fn grant(&self, request: &Self) -> Self {
        let read_only = self.read_only || request.read_only;
        if read_only {
            return Self::read_only();
        }
        let granted = match (&self.granted, &request.granted) {
            (None, None) => None,
            (None, Some(asked)) => Some(asked.clone()),
            (Some(held), None) => Some(held.clone()),
            (Some(held), Some(asked)) => Some(held.intersection(asked).cloned().collect()),
        };
        Self {
            read_only: false,
            granted,
        }
    }

    /// May the holder run this tool? `tool_is_read_only` comes from the tool
    /// itself (`agent_tools::DynTool::is_read_only`), which is the only place
    /// that knows whether a call mutates anything.
    ///
    /// Fail-closed: a mutating tool needs an explicit grant, and no mode, no
    /// prompt and no request can produce one the parent did not hold.
    pub fn allows(&self, tool: &str, tool_is_read_only: bool) -> bool {
        if tool_is_read_only {
            return true;
        }
        if self.read_only {
            return false;
        }
        match &self.granted {
            None => true,
            Some(granted) => granted.contains(tool),
        }
    }

    /// Short label for a listing or a trace. Never carries a tool argument.
    pub fn label(&self) -> String {
        if self.read_only {
            return "read-only".to_string();
        }
        match &self.granted {
            None => "unrestricted".to_string(),
            Some(granted) if granted.is_empty() => "read-only".to_string(),
            Some(granted) => format!(
                "read + {}",
                granted.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

impl Default for AgentAuthority {
    /// Read-only. A forgotten authority must be the smallest one.
    fn default() -> Self {
        Self::read_only()
    }
}

/// A message the parent addressed to a child and the child has not taken yet.
///
/// `send_message` opens no turn, so a queued message is durable state of the
/// PARENT: it survives a restart and is handed to the child the next time one
/// of its turns starts (US-012 AC1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    pub agent_id: AgentId,
    /// Idempotency key. The same one is queued once and delivered once.
    pub message_id: String,
    pub text: String,
}

/// Name of a child a pre-v2 log declared without one: the opaque handle itself.
///
/// An identifier is `agt_<32 hex>`, which is already a valid segment, so a
/// legacy child is addressable by a name that is visibly not a task name rather
/// than by a name the resume invented (edge case #13).
pub fn legacy_name(agent_id: AgentId) -> AgentPath {
    AgentPath::root()
        .join(&agent_id.to_string())
        .unwrap_or_else(|_| AgentPath::root())
}

/// One child in the parent-owned graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    /// Canonical task name (`/root/<task_name>`). What a v2 model addresses the
    /// child by, and what survives a restart when the opaque handle does not.
    pub name: AgentPath,
    /// Durable thread of the child. It survives the child's death.
    pub thread_id: ThreadId,
    /// Task the child was spawned for. Bounded at spawn time.
    pub task: String,
    pub state: AgentState,
    pub authority: AgentAuthority,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    /// Bounded cause of a non-nominal end.
    pub cause: Option<String>,
}

/// Why an agent operation was refused. Every variant refuses BEFORE any
/// provider call and without revealing anything about a thread the caller does
/// not own (US-014 AC4, edge cases #14 and #16).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(
        "sub-agent limit reached: {active}/{MAX_ACTIVE_AGENTS} active, {created}/{MAX_AGENTS_PER_ROOT} created"
    )]
    LimitReached { active: usize, created: usize },
    #[error("a sub-agent cannot spawn a sub-agent (depth limit {MAX_AGENT_DEPTH})")]
    DepthExceeded,
    /// Unknown, terminal, or owned by another parent: ONE error for the three,
    /// so a probe cannot tell them apart.
    #[error("sub-agent {agent_id} is not reachable from this thread")]
    Unreachable { agent_id: AgentId },
    /// Same refusal, for a target named rather than identified.
    #[error("no sub-agent named `{target}` is reachable from this thread")]
    UnknownTarget { target: AgentPath },
    #[error("sub-agent runtime is not attached to a thread")]
    Detached,
    /// A task name already used in this thread. Refused BEFORE anything is
    /// created: reusing a name would make one handle point at two children and
    /// is the shape a spawn cycle takes (US-013 AC3).
    #[error("task name `{name}` is already used by a sub-agent of this thread")]
    NameTaken { name: AgentPath },
    #[error(transparent)]
    InvalidName(#[from] AgentPathError),
    #[error("sub-agent could not be created: {0}")]
    Spawn(String),
    #[error(transparent)]
    Submit(#[from] crate::thread::SubmitError),
}

/// Accounting of the children of ONE thread.
///
/// Interior mutability on purpose: the tools that spawn run inside a turn task,
/// not inside the thread actor, so the reservation has to be atomic on its own
/// rather than by virtue of who calls it.
#[derive(Debug)]
pub struct AgentGraph {
    depth: usize,
    /// Canonical path of the thread that OWNS this graph. Children hang under
    /// it, so a name a model writes is resolved from the caller's position and
    /// never from a global namespace.
    base: AgentPath,
    state: Mutex<GraphState>,
}

#[derive(Debug, Default)]
struct GraphState {
    created: usize,
    records: Vec<AgentRecord>,
}

impl AgentGraph {
    /// Graph of a root thread.
    pub fn new() -> Self {
        Self::at_depth(0)
    }

    /// Graph of a thread sitting `depth` levels below the root. At
    /// [`MAX_AGENT_DEPTH`] every spawn is refused.
    pub fn at_depth(depth: usize) -> Self {
        Self::under(AgentPath::root(), depth)
    }

    /// Graph of the thread sitting at `base`, `depth` levels below the root.
    pub fn under(base: AgentPath, depth: usize) -> Self {
        Self {
            depth,
            base,
            state: Mutex::new(GraphState::default()),
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Canonical path of the owning thread.
    pub fn base(&self) -> &AgentPath {
        &self.base
    }

    /// Canonical name a `task_name` would produce here, validated.
    pub fn canonical_name(&self, task_name: &str) -> Result<AgentPath, AgentError> {
        Ok(self.base.join(task_name)?)
    }

    /// Resolves what a model wrote (`reader`, `/root/reader`) into a child of
    /// this thread. A reference outside this subtree, or one naming an agent
    /// this thread does not own, is `Unreachable` like any foreign handle.
    pub fn resolve(&self, reference: &str) -> Result<AgentRecord, AgentError> {
        let reference = reference.trim();
        if let Ok(agent_id) = AgentId::parse(reference) {
            return self
                .get(agent_id)
                .ok_or(AgentError::Unreachable { agent_id });
        }
        let path = self.base.resolve(reference)?;
        self.by_name(&path)
            .ok_or(AgentError::UnknownTarget { target: path })
    }

    pub fn by_name(&self, name: &AgentPath) -> Option<AgentRecord> {
        self.lock()
            .records
            .iter()
            .find(|record| &record.name == name)
            .cloned()
    }

    /// A poisoned lock is recovered rather than propagated: the accounting is
    /// plain data and losing the whole agent graph is worse than reading a map
    /// a panicking thread left consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, GraphState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reserves a slot, the edge and the identifiers in ONE critical section
    /// (US-012 AC1).
    ///
    /// Refuses on depth, on active children and on lifetime creations, in that
    /// order, and nothing outside this function may add a record: that is what
    /// makes two concurrent spawns for the last slot resolve to exactly one
    /// winner (AC3).
    ///
    /// A reservation consumes a creation even if the caller later reports a
    /// failure. A spawn that fails still burned an attempt, and counting it is
    /// what stops a failing spawn from being retried forever.
    pub fn reserve(
        &self,
        agent_id: AgentId,
        name: AgentPath,
        thread_id: ThreadId,
        task: String,
        authority: AgentAuthority,
        now_ms: u64,
    ) -> Result<(), AgentError> {
        if self.depth >= MAX_AGENT_DEPTH {
            return Err(AgentError::DepthExceeded);
        }
        let mut state = self.lock();
        // Name uniqueness is checked INSIDE the critical section, next to the
        // slot: two spawns racing on the same name must not both believe they
        // own it.
        if state.records.iter().any(|record| record.name == name) {
            return Err(AgentError::NameTaken { name });
        }
        let active = state.records.iter().filter(|r| r.state.is_active()).count();
        if active >= MAX_ACTIVE_AGENTS || state.created >= MAX_AGENTS_PER_ROOT {
            return Err(AgentError::LimitReached {
                active,
                created: state.created,
            });
        }
        state.created += 1;
        state.records.push(AgentRecord {
            agent_id,
            name,
            thread_id,
            task,
            state: AgentState::Running,
            authority,
            started_at_ms: now_ms,
            ended_at_ms: None,
            cause: None,
        });
        Ok(())
    }

    /// Takes a slot back for a child that is idle and receives a follow-up
    /// (US-014 AC1). Bounded by the same concurrency limit as a spawn, and by
    /// no creation budget: resuming a child creates nothing.
    pub fn resume(&self, agent_id: AgentId) -> Result<(), AgentError> {
        let mut state = self.lock();
        let active = state.records.iter().filter(|r| r.state.is_active()).count();
        let created = state.created;
        let Some(record) = state.records.iter_mut().find(|r| r.agent_id == agent_id) else {
            return Err(AgentError::Unreachable { agent_id });
        };
        if !record.state.accepts_input() {
            return Err(AgentError::Unreachable { agent_id });
        }
        if record.state.is_active() {
            // Already running: a steer does not need a second slot.
            return Ok(());
        }
        if active >= MAX_ACTIVE_AGENTS {
            return Err(AgentError::LimitReached { active, created });
        }
        record.state = AgentState::Running;
        record.ended_at_ms = None;
        Ok(())
    }

    /// Records the state a child moved to. Returns `false` when the agent is
    /// unknown or already terminal, so a replayed terminal writes nothing twice
    /// (US-015 AC6).
    pub fn transition(
        &self,
        agent_id: AgentId,
        to: AgentState,
        cause: Option<String>,
        now_ms: u64,
    ) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.iter_mut().find(|r| r.agent_id == agent_id) else {
            return false;
        };
        if record.state.is_terminal() {
            return false;
        }
        record.state = to;
        record.cause = cause;
        record.ended_at_ms = (!to.is_active()).then_some(now_ms);
        true
    }

    pub fn get(&self, agent_id: AgentId) -> Option<AgentRecord> {
        self.lock()
            .records
            .iter()
            .find(|r| r.agent_id == agent_id)
            .cloned()
    }

    /// Every child, in creation order.
    pub fn records(&self) -> Vec<AgentRecord> {
        self.lock().records.clone()
    }

    pub fn active(&self) -> usize {
        self.lock()
            .records
            .iter()
            .filter(|r| r.state.is_active())
            .count()
    }

    pub fn created(&self) -> usize {
        self.lock().created
    }

    /// Rebuilds the graph from a resumed log (US-012 AC1).
    ///
    /// A child the log left RUNNING belongs to a process that no longer exists:
    /// its turn is closed as `interrupted`, with the cause the restart is. A
    /// child left IDLE held no slot and no process, and its own thread log is
    /// durable, so it stays addressable and a follow-up can reopen it. After
    /// this call the active count is zero either way: no phantom slot survives
    /// a restart.
    pub fn restore(&self, records: Vec<AgentRecord>) {
        let mut state = self.lock();
        state.created = records.len();
        state.records = records
            .into_iter()
            .map(|mut record| {
                if record.state.is_active() {
                    record.state = AgentState::Interrupted;
                    record.cause = Some(RESTART_CAUSE.to_string());
                }
                record
            })
            .collect();
    }
}

impl Default for AgentGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{IdGenerator, SequentialIds};

    fn ids() -> SequentialIds {
        SequentialIds::new()
    }

    /// Reserves under a name derived from the generated handle, for the cases
    /// where the name is not what is under test.
    fn reserve(graph: &AgentGraph, ids: &dyn IdGenerator) -> Result<AgentId, AgentError> {
        reserve_as(graph, ids, None)
    }

    /// Reserves under an explicit task name.
    fn named(
        graph: &AgentGraph,
        ids: &dyn IdGenerator,
        task_name: &str,
    ) -> Result<AgentId, AgentError> {
        reserve_as(graph, ids, Some(task_name))
    }

    fn reserve_as(
        graph: &AgentGraph,
        ids: &dyn IdGenerator,
        task_name: Option<&str>,
    ) -> Result<AgentId, AgentError> {
        let agent_id = AgentId::generate(ids);
        let name = match task_name {
            Some(name) => graph.canonical_name(name)?,
            None => graph.canonical_name(&agent_id.to_string())?,
        };
        graph.reserve(
            agent_id,
            name,
            ThreadId::generate(ids),
            "explorer".into(),
            AgentAuthority::read_only(),
            1_000,
        )?;
        Ok(agent_id)
    }

    /// FR-16: every branch of the intersection can only remove. The negative
    /// case of each one is what the security NFR asks for.
    #[test]
    fn a_child_authority_is_never_wider_than_its_parent() {
        let parent = AgentAuthority::with_tools(["edit"]);

        // Asking for a tool the parent does not hold does not create it.
        let child = parent.grant(&AgentAuthority::with_tools(["edit", "bash"]));
        assert!(child.allows("edit", false));
        assert!(!child.allows("bash", false), "bash was never the parent's");

        // Asking for everything from a parent that holds one tool yields one.
        let child = parent.grant(&AgentAuthority::unrestricted());
        assert_eq!(child.granted_tools().map(BTreeSet::len), Some(1));
        assert!(!child.allows("bash", false));

        // A read-only parent cannot produce a mutating child, whatever is asked.
        let child = AgentAuthority::read_only().grant(&AgentAuthority::unrestricted());
        assert!(child.is_read_only());
        assert!(!child.allows("edit", false));
        assert!(child.allows("read", true), "a read stays a read");

        // And a read-only request narrows an unrestricted parent.
        let child = AgentAuthority::unrestricted().grant(&AgentAuthority::read_only());
        assert!(child.is_read_only());
        assert!(!child.allows("write", false));
    }

    /// The default authority is the smallest one: a forgotten field must not
    /// hand a child the parent's rights.
    #[test]
    fn the_default_authority_grants_nothing_that_mutates() {
        let default = AgentAuthority::default();
        assert!(default.is_read_only());
        assert!(!default.allows("bash", false));
        assert!(default.allows("grep", true));
        assert_eq!(default.label(), "read-only");
    }

    /// An authority round-trips through the durable log: the audit of a
    /// transcript must not depend on the process that wrote it.
    #[test]
    fn an_authority_round_trips_through_json() {
        for authority in [
            AgentAuthority::unrestricted(),
            AgentAuthority::read_only(),
            AgentAuthority::with_tools(["edit", "write"]),
        ] {
            let line = serde_json::to_string(&authority).unwrap();
            assert_eq!(
                serde_json::from_str::<AgentAuthority>(&line).unwrap(),
                authority
            );
        }
    }

    /// US-012 AC2, first bound: four active children, and the fifth spawn is
    /// refused before anything is created.
    #[test]
    fn a_fifth_concurrent_child_is_refused() {
        let ids = ids();
        let graph = AgentGraph::new();
        for _ in 0..MAX_ACTIVE_AGENTS {
            reserve(&graph, &ids).expect("a slot is available");
        }
        assert_eq!(graph.active(), MAX_ACTIVE_AGENTS);
        assert!(matches!(
            reserve(&graph, &ids),
            Err(AgentError::LimitReached { active: 4, .. })
        ));
        assert_eq!(
            graph.created(),
            MAX_ACTIVE_AGENTS,
            "a refusal creates nothing"
        );
    }

    /// US-012 AC2, second bound: finishing children frees slots but not the
    /// lifetime budget.
    #[test]
    fn the_ninth_creation_is_refused_even_with_every_slot_free() {
        let ids = ids();
        let graph = AgentGraph::new();
        for _ in 0..MAX_AGENTS_PER_ROOT {
            let agent_id = reserve(&graph, &ids).expect("a slot is available");
            assert!(graph.transition(agent_id, AgentState::Idle, None, 2_000));
        }
        assert_eq!(graph.active(), 0, "an idle child holds no slot");
        assert!(matches!(
            reserve(&graph, &ids),
            Err(AgentError::LimitReached {
                active: 0,
                created: 8
            })
        ));
    }

    /// US-012 AC2, third bound: depth 1 is the floor of the tree.
    #[test]
    fn a_child_cannot_spawn_a_child() {
        let ids = ids();
        let graph = AgentGraph::at_depth(MAX_AGENT_DEPTH);
        assert!(matches!(
            reserve(&graph, &ids),
            Err(AgentError::DepthExceeded)
        ));
        assert_eq!(graph.created(), 0);
    }

    /// US-012 AC4: reporting a failure frees the slot and keeps the cause, but
    /// does not refund the creation.
    #[test]
    fn a_failed_spawn_frees_its_slot_and_keeps_its_cause() {
        let ids = ids();
        let graph = AgentGraph::new();
        let agent_id = reserve(&graph, &ids).unwrap();
        assert!(graph.transition(
            agent_id,
            AgentState::Failed,
            Some("store refused".into()),
            3_000
        ));
        assert_eq!(graph.active(), 0);
        assert_eq!(graph.created(), 1, "a burned attempt stays burned");
        let record = graph.get(agent_id).unwrap();
        assert_eq!(record.state, AgentState::Failed);
        assert_eq!(record.cause.as_deref(), Some("store refused"));
        assert_eq!(record.ended_at_ms, Some(3_000));
        assert!(
            !graph.transition(agent_id, AgentState::Idle, None, 4_000),
            "a terminal agent never moves again"
        );
    }

    /// US-014 AC1: an idle child takes its slot back, and only while one is
    /// free.
    #[test]
    fn an_idle_child_needs_a_free_slot_to_run_again() {
        let ids = ids();
        let graph = AgentGraph::new();
        let first = reserve(&graph, &ids).unwrap();
        assert!(graph.transition(first, AgentState::Idle, None, 2_000));
        for _ in 0..MAX_ACTIVE_AGENTS {
            reserve(&graph, &ids).unwrap();
        }
        assert!(matches!(
            graph.resume(first),
            Err(AgentError::LimitReached { active: 4, .. })
        ));
        assert_eq!(graph.get(first).unwrap().state, AgentState::Idle);
    }

    /// US-014 AC4: an unknown agent and a terminal one are refused by the SAME
    /// error, so a probe learns nothing from the difference.
    #[test]
    fn an_unknown_and_a_terminal_agent_are_refused_alike() {
        let ids = ids();
        let graph = AgentGraph::new();
        let agent_id = reserve(&graph, &ids).unwrap();
        graph.transition(agent_id, AgentState::Interrupted, None, 2_000);

        let stranger = AgentId::generate(&ids);
        assert_eq!(
            graph.resume(agent_id).unwrap_err(),
            AgentError::Unreachable { agent_id }
        );
        assert_eq!(
            graph.resume(stranger).unwrap_err(),
            AgentError::Unreachable { agent_id: stranger }
        );
        assert!(graph.get(stranger).is_none());
    }

    /// US-012 AC1: a graph rebuilt from a log holds no phantom slot. What was
    /// RUNNING is closed with the restart as its cause; what was IDLE keeps its
    /// name and stays addressable, because its own log is durable and a
    /// follow-up must be able to reopen it.
    #[test]
    fn a_restored_graph_closes_what_was_running_and_keeps_an_idle_child_addressable() {
        let ids = ids();
        let graph = AgentGraph::new();
        let records = vec![
            AgentRecord {
                agent_id: AgentId::generate(&ids),
                name: AgentPath::root().join("en_vol").unwrap(),
                thread_id: ThreadId::generate(&ids),
                task: "toujours en vol".into(),
                state: AgentState::Running,
                authority: AgentAuthority::read_only(),
                started_at_ms: 1,
                ended_at_ms: None,
                cause: None,
            },
            AgentRecord {
                agent_id: AgentId::generate(&ids),
                name: AgentPath::root().join("en_attente").unwrap(),
                thread_id: ThreadId::generate(&ids),
                task: "en attente".into(),
                state: AgentState::Idle,
                authority: AgentAuthority::read_only(),
                started_at_ms: 2,
                ended_at_ms: Some(3),
                cause: None,
            },
            AgentRecord {
                agent_id: AgentId::generate(&ids),
                name: AgentPath::root().join("close").unwrap(),
                thread_id: ThreadId::generate(&ids),
                task: "déjà close".into(),
                state: AgentState::Failed,
                authority: AgentAuthority::read_only(),
                started_at_ms: 4,
                ended_at_ms: Some(5),
                cause: Some("provider".into()),
            },
        ];
        graph.restore(records);

        assert_eq!(graph.active(), 0, "no phantom slot after a restart");
        assert_eq!(graph.created(), 3, "the creation budget survives");
        let states: Vec<AgentState> = graph.records().iter().map(|r| r.state).collect();
        assert_eq!(
            states,
            vec![
                AgentState::Interrupted,
                AgentState::Idle,
                AgentState::Failed
            ]
        );
        assert_eq!(graph.records()[0].cause.as_deref(), Some(RESTART_CAUSE));
        assert_eq!(
            graph.records()[2].cause.as_deref(),
            Some("provider"),
            "a cause already recorded is not rewritten"
        );

        // The canonical names survive, so a model addresses the same child by
        // the same handle across a restart.
        let idle = AgentPath::root().join("en_attente").unwrap();
        let restored = graph
            .resolve("en_attente")
            .expect("the name still resolves");
        assert_eq!(restored.name, idle);
        assert!(restored.state.accepts_input());
        assert!(matches!(
            graph.resolve("en_vol").map(|record| record.state),
            Ok(AgentState::Interrupted)
        ));
        assert!(matches!(
            graph.resolve("jamais_cree"),
            Err(AgentError::UnknownTarget { .. })
        ));
    }

    /// A task name is a handle: two children of one thread may not share it,
    /// and the refusal costs neither a slot nor a creation (US-013 AC3).
    #[test]
    fn a_reused_task_name_is_refused_without_burning_a_creation() {
        let ids = ids();
        let graph = AgentGraph::new();
        named(&graph, &ids, "reader").expect("the first name is free");
        assert!(matches!(
            named(&graph, &ids, "reader"),
            Err(AgentError::NameTaken { .. })
        ));
        assert_eq!(graph.created(), 1, "a refused name creates nothing");
        assert_eq!(graph.active(), 1);

        // And a name that is not a name at all never reaches the graph.
        assert!(matches!(
            named(&graph, &ids, "Reader"),
            Err(AgentError::InvalidName(_))
        ));
        assert!(matches!(
            named(&graph, &ids, "root"),
            Err(AgentError::InvalidName(_))
        ));
        assert_eq!(graph.created(), 1);
    }

    /// A reference is resolved from the graph's OWN base path, so a child of
    /// another thread is never reachable by name.
    #[test]
    fn a_name_resolves_only_inside_the_subtree_that_owns_it() {
        let ids = ids();
        let graph = AgentGraph::under(AgentPath::root().join("branch").unwrap(), 0);
        let agent_id = named(&graph, &ids, "reader").unwrap();
        assert_eq!(
            graph.resolve("reader").unwrap().agent_id,
            agent_id,
            "a relative reference is resolved from the owner"
        );
        assert_eq!(
            graph.resolve("/root/branch/reader").unwrap().agent_id,
            agent_id
        );
        assert!(matches!(
            graph.resolve("/root/reader"),
            Err(AgentError::UnknownTarget { .. })
        ));
        assert!(matches!(
            graph.resolve("agt_00000000000000000000000000000099"),
            Err(AgentError::Unreachable { .. })
        ));
    }

    /// US-012 AC3: whichever thread wins, exactly one does.
    #[test]
    fn concurrent_reservations_for_the_last_slot_produce_one_winner() {
        let graph = std::sync::Arc::new(AgentGraph::new());
        let ids = std::sync::Arc::new(SequentialIds::new());
        for _ in 0..MAX_ACTIVE_AGENTS - 1 {
            reserve(&graph, ids.as_ref()).unwrap();
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let winners: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let graph = std::sync::Arc::clone(&graph);
                    let ids = std::sync::Arc::clone(&ids);
                    let barrier = std::sync::Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        reserve(&graph, ids.as_ref()).is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(
            winners.iter().filter(|won| **won).count(),
            1,
            "exactly one reservation may take the last slot"
        );
        assert_eq!(graph.active(), MAX_ACTIVE_AGENTS);
        assert_eq!(graph.created(), MAX_ACTIVE_AGENTS);
    }
}
