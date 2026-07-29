//! One reading of a terminal turn cause, shared by every surface (US-019 AC1).
//!
//! The problem this closes: a turn ends `failed`, the durable log records a
//! cause, and each surface used to decide on its own what to do with that
//! string. The TUI dropped it entirely, headless kept it raw, the app-server
//! forwarded it, and the JSONL summary had no vocabulary for it. Four surfaces,
//! four answers, and a user who cannot tell whether the model, the network, the
//! credential or a guardrail stopped the turn.
//!
//! The fix is deliberately NOT a schema change. `cause` stays the bounded string
//! the log already carries, so every session file written before this module
//! stays readable and no migration touches a user journal (compatibility NFR).
//! What is shared is the READING: [`TurnFailure::classify`] maps a cause onto a
//! closed [`FailureCategory`], and every surface renders the same category, the
//! same actionable next step and the same identifiers.
//!
//! Classification is by prefix because the causes ARE prefixed at their emission
//! sites, and those prefixes are part of what the log means:
//! `provider: `/`auth: `/`compaction: `/`session: `/`invalid request: ` come
//! from [`agent_core::error::AgentError`], `exhausted: ` from a guardrail stop,
//! `interrupted: `/`shutdown` from a cancellation or a resume repair, and
//! `model runtime refused: ` from a local capability refusal. Anything else is
//! [`FailureCategory::Unknown`], which is a visible admission rather than a
//! wrong guess.

use crate::lifecycle::TurnState;

/// Closed set of terminal-cause categories. Each variant names a DIFFERENT next
/// diagnostic, which is the only reason for a category to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// The provider did not deliver a usable turn (transport, HTTP, stream).
    Provider,
    /// The credential was refused or could not be renewed.
    Auth,
    /// The transcript could not be brought back under the context budget.
    Context,
    /// The model asked for something the request could not carry.
    InvalidRequest,
    /// The local capability resolution refused the model before any request.
    ModelRuntime,
    /// A guardrail stopped the loop; the work did not finish.
    Guardrail,
    /// A cancellation, a shutdown or a resume repair closed the turn.
    Interrupted,
    /// The durable log could not be read or written.
    Store,
    /// A cause the classifier does not recognize, reported as such.
    Unknown,
}

impl FailureCategory {
    /// Stable wire label. Shared by the JSONL summary and the app-server, so a
    /// client matches on one vocabulary.
    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Auth => "auth",
            Self::Context => "context",
            Self::InvalidRequest => "invalid_request",
            Self::ModelRuntime => "model_runtime",
            Self::Guardrail => "guardrail",
            Self::Interrupted => "interrupted",
            Self::Store => "store",
            Self::Unknown => "unknown",
        }
    }

    /// The next thing to actually do. One sentence, imperative, identical on
    /// every surface: a category without a next step is a label, not a
    /// diagnosis.
    pub fn guidance(self) -> &'static str {
        match self {
            Self::Provider => "retry the turn; check connectivity if it repeats",
            Self::Auth => "reconnect the ChatGPT subscription with /login",
            Self::Context => "start a new thread or /rewind before retrying",
            Self::InvalidRequest => "report this request; the turn cannot be replayed as is",
            Self::ModelRuntime => {
                "select a model this build supports, or install the missing component"
            }
            Self::Guardrail => "raise the limit the cause names, or split the task",
            Self::Interrupted => "resume the thread and submit the turn again",
            Self::Store => "check the session file and free space, then reopen the thread",
            Self::Unknown => "read the cause below and the trace under PYXIS_LOG=debug",
        }
    }
}

/// A terminal cause, read once. Cheap to build and free of allocation beyond the
/// message it borrows into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub category: FailureCategory,
    /// The cause as the log recorded it, minus the prefix that named the
    /// category. Never empty: a category with no message repeats its label.
    pub message: String,
}

impl TurnFailure {
    /// Reads a terminal state and its recorded cause.
    ///
    /// `None` for a state that is not a failure: `completed` has nothing to
    /// diagnose, and an `interrupted` turn WITHOUT a cause was stopped by the
    /// user, which is not a fault to report.
    pub fn classify(state: TurnState, cause: Option<&str>) -> Option<Self> {
        match state {
            TurnState::Completed => None,
            TurnState::Interrupted => cause.map(Self::from_cause),
            TurnState::Failed => Some(match cause {
                Some(cause) => Self::from_cause(cause),
                // A failure the log did not explain is still a failure, and
                // saying so beats an empty line.
                None => Self {
                    category: FailureCategory::Unknown,
                    message: "the turn failed without a recorded cause".to_string(),
                },
            }),
            _ => None,
        }
    }

    /// Reads a cause string on its own, for a surface that already knows it is
    /// looking at a failure (a store fault, a run summary).
    pub fn from_cause(cause: &str) -> Self {
        // Order matters: a longer prefix comes before the shorter one it starts
        // with, so `shutdown: task aborted` is not swallowed by `shutdown`.
        //
        // A prefix ending in `": "` is a LABEL and is stripped, because
        // repeating it inside the message would print the category twice. Any
        // other prefix only classifies: it is part of the sentence, and cutting
        // it would leave a fragment that reads as a different failure.
        const PREFIXES: &[(&str, FailureCategory)] = &[
            ("provider: ", FailureCategory::Provider),
            ("auth: ", FailureCategory::Auth),
            ("compaction: ", FailureCategory::Context),
            (
                "unrecoverable context (compaction failed): ",
                FailureCategory::Context,
            ),
            ("invalid request: ", FailureCategory::InvalidRequest),
            ("model runtime refused: ", FailureCategory::ModelRuntime),
            ("exhausted: ", FailureCategory::Guardrail),
            ("interrupted: ", FailureCategory::Interrupted),
            ("shutdown: ", FailureCategory::Interrupted),
            ("shutdown", FailureCategory::Interrupted),
            ("session: ", FailureCategory::Store),
            ("thread store", FailureCategory::Store),
        ];
        let trimmed = cause.trim();
        for (prefix, category) in PREFIXES {
            let Some(rest) = trimmed.strip_prefix(prefix) else {
                continue;
            };
            let message = match prefix.ends_with(": ") {
                true => rest.trim(),
                false => trimmed,
            };
            return Self {
                category: *category,
                // A label with nothing behind it repeats itself rather than
                // showing an empty message.
                message: if message.is_empty() {
                    trimmed.to_string()
                } else {
                    message.to_string()
                },
            };
        }
        Self {
            category: FailureCategory::Unknown,
            message: trimmed.to_string(),
        }
    }

    /// The line every surface prints: category, message, next step. Identifiers
    /// are added by the caller, which is the only one that knows which of them
    /// its surface already shows.
    pub fn summary(&self) -> String {
        format!(
            "{}: {} — {}",
            self.category.label(),
            self.message,
            self.category.guidance()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC1: the four surfaces read the SAME category out of the causes the
    /// runtime actually emits. Each line below is a cause built by a real
    /// emission site, not an invented one.
    #[test]
    fn every_cause_the_runtime_emits_lands_on_its_category() {
        let cases: &[(&str, FailureCategory, &str)] = &[
            (
                "provider: provider transport failed",
                FailureCategory::Provider,
                "provider transport failed",
            ),
            ("auth: Expired", FailureCategory::Auth, "Expired"),
            (
                "compaction: circuit breaker (3 consecutive failures)",
                FailureCategory::Context,
                "circuit breaker (3 consecutive failures)",
            ),
            (
                "unrecoverable context (compaction failed): nothing left to drop",
                FailureCategory::Context,
                "nothing left to drop",
            ),
            (
                "invalid request: tool result without a call",
                FailureCategory::InvalidRequest,
                "tool result without a call",
            ),
            (
                "model runtime refused: code mode runtime unavailable",
                FailureCategory::ModelRuntime,
                "code mode runtime unavailable",
            ),
            (
                "exhausted: max steps reached",
                FailureCategory::Guardrail,
                "max steps reached",
            ),
            (
                "interrupted: recovered from `running` at resume",
                FailureCategory::Interrupted,
                "recovered from `running` at resume",
            ),
            ("shutdown", FailureCategory::Interrupted, "shutdown"),
            (
                "shutdown: task aborted",
                FailureCategory::Interrupted,
                "task aborted",
            ),
            (
                "session: log unreadable",
                FailureCategory::Store,
                "log unreadable",
            ),
            (
                "thread store failed during append: disk full",
                FailureCategory::Store,
                "thread store failed during append: disk full",
            ),
        ];
        for (cause, category, message) in cases {
            let failure = TurnFailure::from_cause(cause);
            assert_eq!(failure.category, *category, "cause: {cause}");
            assert_eq!(failure.message, *message, "cause: {cause}");
        }
    }

    /// A prefix that only CLASSIFIES keeps the whole sentence; one that is a
    /// label is cut. Cutting `thread store` would leave "failed during append",
    /// which reads as a different fault; cutting `provider: ` leaves exactly the
    /// provider's own message.
    #[test]
    fn only_a_label_prefix_is_stripped_from_the_message() {
        let classified = TurnFailure::from_cause("thread store failed during close: EIO");
        assert_eq!(classified.category, FailureCategory::Store);
        assert_eq!(classified.message, "thread store failed during close: EIO");

        let labelled = TurnFailure::from_cause("provider: 429 from upstream");
        assert_eq!(labelled.message, "429 from upstream");
    }

    /// A longer prefix wins over the shorter one it starts with: `shutdown: `
    /// is a label, `shutdown` alone is the whole cause.
    #[test]
    fn a_longer_prefix_is_not_swallowed_by_the_shorter_one() {
        assert_eq!(TurnFailure::from_cause("shutdown").message, "shutdown");
        assert_eq!(
            TurnFailure::from_cause("shutdown: task aborted").message,
            "task aborted"
        );
    }

    /// An unrecognized cause is REPORTED as unrecognized. Guessing a category
    /// would send the user to the wrong diagnostic, which is worse than saying
    /// the classifier does not know.
    #[test]
    fn an_unknown_cause_is_named_unknown_and_keeps_its_text() {
        let failure = TurnFailure::from_cause("le moteur a rendu l'âme");
        assert_eq!(failure.category, FailureCategory::Unknown);
        assert_eq!(failure.message, "le moteur a rendu l'âme");
        assert!(failure.summary().contains("PYXIS_LOG=debug"));
    }

    #[test]
    fn a_completed_turn_has_nothing_to_diagnose() {
        assert!(TurnFailure::classify(TurnState::Completed, None).is_none());
        assert!(TurnFailure::classify(TurnState::Completed, Some("provider: x")).is_none());
        // A user interruption records no cause: nothing to report either.
        assert!(TurnFailure::classify(TurnState::Interrupted, None).is_none());
    }

    #[test]
    fn a_failure_without_a_cause_is_still_reported() {
        let failure = TurnFailure::classify(TurnState::Failed, None).expect("failed is a failure");
        assert_eq!(failure.category, FailureCategory::Unknown);
        assert!(failure.message.contains("without a recorded cause"));
    }

    /// Every category names a distinct next step: a guidance shared by two
    /// categories would mean one of them does not deserve to exist.
    #[test]
    fn categories_have_distinct_labels_and_guidance() {
        const ALL: &[FailureCategory] = &[
            FailureCategory::Provider,
            FailureCategory::Auth,
            FailureCategory::Context,
            FailureCategory::InvalidRequest,
            FailureCategory::ModelRuntime,
            FailureCategory::Guardrail,
            FailureCategory::Interrupted,
            FailureCategory::Store,
            FailureCategory::Unknown,
        ];
        let mut labels: Vec<&str> = ALL.iter().map(|c| c.label()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "labels must be unique");

        let mut guidance: Vec<&str> = ALL.iter().map(|c| c.guidance()).collect();
        guidance.sort_unstable();
        guidance.dedup();
        assert_eq!(guidance.len(), count, "guidance must be unique");
    }

    /// The rendered line carries the three things a surface owes the user, in
    /// the same order everywhere.
    #[test]
    fn the_summary_carries_category_message_and_next_step() {
        let failure = TurnFailure::from_cause("provider: 503 from upstream");
        assert_eq!(
            failure.summary(),
            "provider: 503 from upstream — retry the turn; check connectivity if it repeats"
        );
    }
}
