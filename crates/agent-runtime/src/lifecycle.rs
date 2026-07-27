//! Turn lifecycle (US-002, FR-04): six states, one terminal per turn.
//!
//! The guard lives here rather than in the actor so that "exactly one terminal
//! state" is a property of a type, not of a code path. A refused transition is
//! a typed error, and the actor persists NOTHING when it gets one: that is what
//! keeps a double interruption from writing two terminals (edge case #6/#8).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::TurnId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// Accepted and durable, no engine running yet.
    Queued,
    /// The turn engine owns the turn.
    Running,
    /// The turn is waiting for a client decision (permission ask).
    NeedsInput,
    /// Terminal: the engine reached the end of the turn.
    Completed,
    /// Terminal: cancelled, after reconciliation and cleanup.
    Interrupted,
    /// Terminal: provider, store or tool pipeline failure.
    Failed,
}

impl TurnState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsInput => "needs_input",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }

    fn can_move_to(self, to: Self) -> bool {
        match self {
            Self::Queued => matches!(
                to,
                Self::Running | Self::Interrupted | Self::Failed | Self::Completed
            ),
            Self::Running => matches!(
                to,
                Self::NeedsInput | Self::Completed | Self::Interrupted | Self::Failed
            ),
            Self::NeedsInput => matches!(
                to,
                Self::Running | Self::Completed | Self::Interrupted | Self::Failed
            ),
            Self::Completed | Self::Interrupted | Self::Failed => false,
        }
    }
}

impl fmt::Display for TurnState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    /// A terminal was already written for this turn. Refusing here is what
    /// makes a second interruption idempotent instead of duplicating.
    #[error(
        "turn {turn_id} is already terminal in `{current}`: refused transition to `{requested}`"
    )]
    AlreadyTerminal {
        turn_id: TurnId,
        current: TurnState,
        requested: TurnState,
    },
    #[error("illegal transition for turn {turn_id}: `{from}` -> `{to}`")]
    Illegal {
        turn_id: TurnId,
        from: TurnState,
        to: TurnState,
    },
}

/// State of a single turn. Owned by the thread actor; never shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnLifecycle {
    turn_id: TurnId,
    state: TurnState,
}

impl TurnLifecycle {
    /// A turn exists as soon as its input is durable, hence `queued`.
    pub fn queued(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            state: TurnState::Queued,
        }
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn state(&self) -> TurnState {
        self.state
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Applies a transition and returns the state it came from, so the caller
    /// can persist `from` and `to` in the same event. Errors leave the state
    /// untouched.
    pub fn transition(&mut self, to: TurnState) -> Result<TurnState, LifecycleError> {
        if self.state.is_terminal() {
            return Err(LifecycleError::AlreadyTerminal {
                turn_id: self.turn_id,
                current: self.state,
                requested: to,
            });
        }
        if !self.state.can_move_to(to) {
            return Err(LifecycleError::Illegal {
                turn_id: self.turn_id,
                from: self.state,
                to,
            });
        }
        let from = self.state;
        self.state = to;
        Ok(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SequentialIds;

    fn turn() -> TurnLifecycle {
        TurnLifecycle::queued(TurnId::generate(&SequentialIds::new()))
    }

    #[test]
    fn a_turn_walks_queued_running_completed() {
        let mut t = turn();
        assert_eq!(t.state(), TurnState::Queued);
        assert_eq!(t.transition(TurnState::Running).unwrap(), TurnState::Queued);
        assert_eq!(
            t.transition(TurnState::Completed).unwrap(),
            TurnState::Running
        );
        assert!(t.is_terminal());
    }

    #[test]
    fn needs_input_returns_to_running() {
        let mut t = turn();
        t.transition(TurnState::Running).unwrap();
        t.transition(TurnState::NeedsInput).unwrap();
        assert_eq!(
            t.transition(TurnState::Running).unwrap(),
            TurnState::NeedsInput
        );
    }

    #[test]
    fn a_second_terminal_is_refused_with_a_typed_error() {
        let mut t = turn();
        t.transition(TurnState::Running).unwrap();
        t.transition(TurnState::Interrupted).unwrap();
        let err = t.transition(TurnState::Failed).unwrap_err();
        assert!(matches!(
            err,
            LifecycleError::AlreadyTerminal {
                current: TurnState::Interrupted,
                requested: TurnState::Failed,
                ..
            }
        ));
        // The refused transition changed nothing: the turn keeps its terminal.
        assert_eq!(t.state(), TurnState::Interrupted);
        assert!(matches!(
            t.transition(TurnState::Interrupted).unwrap_err(),
            LifecycleError::AlreadyTerminal { .. }
        ));
    }

    #[test]
    fn an_illegal_transition_is_refused_without_changing_state() {
        let mut t = turn();
        t.transition(TurnState::Running).unwrap();
        assert!(matches!(
            t.transition(TurnState::Running).unwrap_err(),
            LifecycleError::Illegal {
                from: TurnState::Running,
                to: TurnState::Running,
                ..
            }
        ));
        assert_eq!(t.state(), TurnState::Running);
    }

    #[test]
    fn terminality_is_explicit() {
        for state in [
            TurnState::Completed,
            TurnState::Interrupted,
            TurnState::Failed,
        ] {
            assert!(state.is_terminal(), "{state} must be terminal");
        }
        for state in [TurnState::Queued, TurnState::Running, TurnState::NeedsInput] {
            assert!(!state.is_terminal(), "{state} must not be terminal");
        }
    }

    #[test]
    fn states_round_trip_through_json() {
        let json = serde_json::to_string(&TurnState::NeedsInput).unwrap();
        assert_eq!(json, "\"needs_input\"");
        assert_eq!(
            serde_json::from_str::<TurnState>(&json).unwrap(),
            TurnState::NeedsInput
        );
        assert!(serde_json::from_str::<TurnState>("\"pending\"").is_err());
    }
}
