//! `TurnRunner`: the ONLY seam between the thread runtime and the turn engine
//! (US-001).
//!
//! `run_agent` stays the single model-tools loop. The runtime never re-implements
//! retry, compaction or tool dispatch: it hands a turn to a runner, forwards the
//! engine events untouched, and reads back exactly one outcome. `RunAgentRunner`
//! is the production implementation and it is thin on purpose: the day it needs
//! to grow a loop of its own, US-001's escape hatch applies and the architecture
//! is wrong.

use std::sync::Arc;

use agent_core::event::AgentEvent;
use agent_core::input::InputQueue;
use agent_core::model::ResolvedModelRuntime;
use agent_core::{AgentContext, Deps, run_agent};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::context::TurnContext;
use crate::id::TurnId;
use crate::inputs::TurnInputs;
use crate::lifecycle::TurnState;

/// What the runtime hands to the engine for one turn.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub turn_id: TurnId,
    /// User input that opened the turn.
    pub text: String,
    /// Configuration this turn is committed to until its terminal state
    /// (US-006). A context factory reads it instead of re-reading live settings,
    /// which is what keeps a mid-turn change from splitting the turn in two.
    pub context: TurnContext,
    /// In-memory body referenced by `context.model_runtime_fingerprint`.
    pub model_runtime: Option<ResolvedModelRuntime>,
    pub overload_fallback_runtime: Option<ResolvedModelRuntime>,
    /// Steering inputs accepted for THIS turn (US-007). Owned by the actor,
    /// consumed by the engine at its safe points.
    pub inputs: Arc<TurnInputs>,
}

/// How a turn ended, as reported by the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// `AgentEvent::EndTurn`.
    Completed,
    /// `AgentEvent::Interrupted`, after the engine reconciled its transcript.
    Interrupted,
    /// `AgentEvent::Exhausted`: a guardrail stopped the loop.
    Exhausted(String),
    /// `AgentEvent::Error`, or an engine stream that ended without a terminal.
    Failed(String),
}

impl TurnOutcome {
    /// Terminal state this outcome maps to.
    ///
    /// `Exhausted` maps to `failed`, not `completed`: a guardrail stop did not
    /// finish the work, and a client must never read it as a success. The
    /// reason survives in `cause`.
    pub fn terminal_state(&self) -> TurnState {
        match self {
            Self::Completed => TurnState::Completed,
            Self::Interrupted => TurnState::Interrupted,
            Self::Exhausted(_) | Self::Failed(_) => TurnState::Failed,
        }
    }

    /// Bounded cause recorded next to the terminal state.
    pub fn cause(&self) -> Option<String> {
        match self {
            Self::Completed | Self::Interrupted => None,
            Self::Exhausted(reason) => Some(format!("exhausted: {reason}")),
            Self::Failed(reason) => Some(reason.clone()),
        }
    }
}

/// Runs one turn. Implementations MUST forward every engine event to `events`
/// unmodified and MUST return once, after the engine reached its terminal.
#[async_trait::async_trait]
pub trait TurnRunner: Send + Sync {
    async fn run_turn(
        &self,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnOutcome;
}

/// Production runner: drives `agent_core::run_agent`.
///
/// The context factory builds the `AgentContext` of a turn from its
/// [`TurnContext`], and is where a client attaches its own
/// `agent_core::StepContextSource` (US-006). The runner itself only wires what
/// belongs to the runtime contract: the turn's cancellation node and its
/// steering queue.
pub struct RunAgentRunner<F> {
    deps: Deps,
    context: F,
}

impl<F> RunAgentRunner<F>
where
    F: Fn(&TurnRequest) -> AgentContext + Send + Sync,
{
    pub fn new(deps: Deps, context: F) -> Self {
        Self { deps, context }
    }
}

fn outcome_of(event: &AgentEvent) -> Option<TurnOutcome> {
    match event {
        AgentEvent::EndTurn => Some(TurnOutcome::Completed),
        AgentEvent::Interrupted(..) => Some(TurnOutcome::Interrupted),
        AgentEvent::Exhausted(reason) => Some(TurnOutcome::Exhausted(format!("{reason:?}"))),
        AgentEvent::Error(err) => Some(TurnOutcome::Failed(err.to_string())),
        _ => None,
    }
}

#[async_trait::async_trait]
impl<F> TurnRunner for RunAgentRunner<F>
where
    F: Fn(&TurnRequest) -> AgentContext + Send + Sync,
{
    async fn run_turn(
        &self,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let mut ctx = (self.context)(&request);
        ctx.turn_id = Some(request.turn_id.to_string());
        // US-007: the queue the actor fills is the queue the engine drains. The
        // runner wires it rather than the factory, so no client can forget it and
        // silently turn every steer into a post-turn message.
        ctx.inputs = Some(Arc::clone(&request.inputs) as Arc<dyn InputQueue>);

        // US-008 AC1: the turn's token is handed to the engine AS IS. There is no
        // second signal to bridge anymore, so no branch of the tree can be
        // cancelled while the other keeps running.
        let mut deps = self.deps.clone();
        deps.cancel = cancel;

        let stream = run_agent(ctx, deps);
        futures_util::pin_mut!(stream);

        let mut outcome = TurnOutcome::Failed("engine ended without a terminal event".into());
        while let Some(event) = stream.next().await {
            if let Some(terminal) = outcome_of(&event) {
                outcome = terminal;
            }
            // A disconnected client must not stop the engine: the loop still has
            // to reconcile its transcript and persist.
            let _ = events.send(event).await;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_map_to_exactly_one_terminal_state() {
        assert_eq!(
            TurnOutcome::Completed.terminal_state(),
            TurnState::Completed
        );
        assert_eq!(
            TurnOutcome::Interrupted.terminal_state(),
            TurnState::Interrupted
        );
        assert_eq!(
            TurnOutcome::Exhausted("ToolLoop { count: 4 }".into()).terminal_state(),
            TurnState::Failed
        );
        assert_eq!(
            TurnOutcome::Failed("boom".into()).terminal_state(),
            TurnState::Failed
        );
    }

    #[test]
    fn only_non_nominal_outcomes_carry_a_cause() {
        assert_eq!(TurnOutcome::Completed.cause(), None);
        assert_eq!(TurnOutcome::Interrupted.cause(), None);
        assert_eq!(
            TurnOutcome::Exhausted("ToolLoop { count: 4 }".into()).cause(),
            Some("exhausted: ToolLoop { count: 4 }".into())
        );
        assert_eq!(
            TurnOutcome::Failed("boom".into()).cause(),
            Some("boom".into())
        );
    }

    #[test]
    fn engine_events_that_are_not_terminal_produce_no_outcome() {
        assert!(outcome_of(&AgentEvent::Text("salut".into())).is_none());
        assert!(outcome_of(&AgentEvent::StreamReset).is_none());
        assert!(outcome_of(&AgentEvent::EndTurn).is_some());
    }
}
