//! One rendering of a terminal turn failure, for the two surfaces the binary
//! owns (US-019 AC1).
//!
//! `agent_runtime::TurnFailure` decides the category, the message and the next
//! step. This module only decides how the identifiers are appended, and it lives
//! here rather than in the runtime because appending them is a presentation
//! choice: the app-server carries `threadId` and `turnId` as protocol FIELDS and
//! must not repeat them inside a sentence, while a terminal transcript and a
//! stderr line have nowhere else to put them.

use std::fmt::Display;

use agent_runtime::TurnFailure;

/// `category: message — next step (thread <id>, turn <id>)`.
///
/// The identifiers close the loop between a screen and a session file: without
/// them a user reading a failure in the TUI cannot find the same turn in
/// `~/.pyxis/sessions` or in an app-server stream.
pub fn render(
    failure: &TurnFailure,
    thread_id: impl Display,
    turn_id: Option<impl Display>,
) -> String {
    match turn_id {
        Some(turn_id) => format!("{} (thread {thread_id}, turn {turn_id})", failure.summary()),
        None => format!("{} (thread {thread_id})", failure.summary()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::{SequentialIds, ThreadId, TurnId};

    #[test]
    fn the_line_carries_the_category_the_next_step_and_both_identifiers() {
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let turn_id = TurnId::generate(&ids);
        let failure = TurnFailure::from_cause("provider: stream closed before the terminal");

        let line = render(&failure, thread_id, Some(turn_id));

        assert!(line.starts_with("provider: "), "{line}");
        assert!(line.contains("stream closed before the terminal"), "{line}");
        assert!(line.contains("retry the turn"), "{line}");
        assert!(line.contains(&thread_id.to_string()), "{line}");
        assert!(line.contains(&turn_id.to_string()), "{line}");
    }

    /// An event that names no turn still names its thread: a half-identified
    /// failure is still findable.
    #[test]
    fn without_a_turn_the_thread_is_still_named() {
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let line = render(
            &TurnFailure::from_cause("shutdown"),
            thread_id,
            None::<TurnId>,
        );
        assert!(line.contains(&thread_id.to_string()), "{line}");
        assert!(!line.contains(", turn "), "{line}");
    }
}
