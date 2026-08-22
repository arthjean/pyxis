//! Rebuilding a thread from its durable log (US-009).
//!
//! Resume is a PURE function of the log plus, at most, one recovery event. It
//! answers three questions the actor cannot answer from process-local state:
//! where the conversation stopped, which turn a crash left without a terminal,
//! and which client submissions were already accepted.
//!
//! Two invariants live here:
//! - every turn ends up terminal. A turn the log left `queued`, `running` or
//!   `needs_input` is one no engine will ever finish, so resume closes it as
//!   `interrupted` exactly once (FR-04, edge case #11).
//! - a resumed transcript carries no orphan `tool_use`. The engine reconciles
//!   before persisting, but a crash gets no chance to; resume completes what is
//!   missing so the next provider call is not rejected outright.

use std::collections::HashMap;

use agent_core::message::{INTERRUPTED_TOOL_RESULT, Message, ToolErrorKind, unanswered_tool_calls};

use crate::agent::{AgentMessage, AgentRecord, AgentState};
use crate::context::TurnContext;
use crate::event::{ForkOrigin, ThreadEventPayload};
use crate::id::{ThreadId, TurnId};
use crate::jobs::{JobRecord, JobStatus};
use crate::lifecycle::TurnState;
use crate::schedule::{FoldedSchedules, ScheduleChange, ScheduleRecord, fold_schedules};
use crate::store::ThreadSnapshot;
use crate::thread::{Accepted, InputOrigin, Submission, TurnStatus};

/// State a client gets back when it opens an existing thread.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumedThread {
    pub thread_id: ThreadId,
    /// Transcript rebuilt from the log, reconciled: no tool call is left
    /// without a result.
    pub messages: Vec<Message>,
    /// Last turn of the thread and the state it ended in.
    pub turn: Option<TurnStatus>,
    /// Configuration the last started turn was committed to. `None` for a
    /// thread that never started one, or for a log written before turn
    /// configurations were durable.
    pub turn_context: Option<TurnContext>,
    /// Turns a previous run left without a terminal and that resume closed.
    pub recovered: Vec<TurnId>,
    /// Tool calls resume had to answer synthetically.
    pub reconciled_calls: usize,
    /// Submissions already accepted, by client idempotency key (FR-12).
    pub accepted: HashMap<String, Accepted>,
    /// Branch this thread was cut from, when it is one.
    pub origin: Option<ForkOrigin>,
    /// Children this thread declared, in log order, with the last state each
    /// one reached. A child the log left open is reported here as it was and
    /// closed by [`crate::agent::AgentGraph::restore`]: the plan describes the
    /// log, the graph decides what a restart makes of it (US-012 AC5).
    pub agents: Vec<AgentRecord>,
    /// Messages queued on a child and not delivered yet, in log order
    /// (US-012 AC1). A delivered one is absent: the log says it was taken.
    pub agent_messages: Vec<AgentMessage>,
    /// Background jobs this thread registered, in log order, with the last
    /// state and the report flag each one reached. Nothing is relaunched: the
    /// plan describes the log, and a job whose process is gone stays what the
    /// log says it was (EP-041).
    pub jobs: Vec<JobRecord>,
    /// Reminders this thread holds, REBUILT by folding the log against the
    /// instant it was reopened at (US-153). Nothing here was submitted and no
    /// turn was opened for it: a resume reports, and delivering a late reminder
    /// belongs to the live actor and to its timer arm (ADR-17).
    pub schedules: FoldedSchedules,
}

impl ResumedThread {
    /// True when the log held nothing yet.
    pub fn is_new(&self) -> bool {
        self.turn.is_none() && self.messages.is_empty()
    }
}

/// What the actor has to persist before it starts serving commands.
pub(crate) struct ResumePlan {
    pub(crate) resumed: ResumedThread,
    /// Running turns to close, with the state they were left in. In log order.
    pub(crate) unfinished: Vec<(TurnId, TurnState)>,
    /// Durable queued inputs to promote after the repair is committed.
    pub(crate) queued: Vec<(TurnId, Submission)>,
    /// Messages the recovery commit must add to the canonical transcript.
    pub(crate) tool_results: Vec<Message>,
    pub(crate) thread_created: bool,
    /// The scheduling changes of the log, in order, so the actor can re-fold
    /// them against a later instant. The folded state on `resumed` is a REPORT
    /// settled at the instant of the resume (ADR-17); the live timer needs the
    /// sequence itself, and rebuilding it from the snapshot a second time would
    /// duplicate the mapping this pass already did.
    pub(crate) schedule_changes: Vec<ScheduleChange>,
}

/// Replays `snapshot` into the state a thread resumes on.
///
/// Does not mutate anything: the caller decides what to persist, so a read-only
/// inspection of a thread never writes a recovery event.
pub(crate) fn plan(thread_id: ThreadId, snapshot: &ThreadSnapshot, now_ms: u64) -> ResumePlan {
    // Ordered by first appearance so `recovered` and the last known turn follow
    // the log and not a hash order.
    let mut states: Vec<(TurnId, TurnState)> = Vec::new();
    let mut accepted: HashMap<String, Accepted> = HashMap::new();
    let mut submitted: Vec<(TurnId, Submission)> = Vec::new();
    let mut agents: Vec<AgentRecord> = Vec::new();
    let mut agent_messages: Vec<AgentMessage> = Vec::new();
    let mut jobs: Vec<JobRecord> = Vec::new();
    // Collected in log order and folded ONCE at the end: the fold is a function
    // of the whole sequence, so replaying it entry by entry here would rebuild
    // a second, mutable projection of exactly what it already computes.
    let mut schedules: Vec<ScheduleChange> = Vec::new();
    let mut turn_context: Option<TurnContext> = None;
    let mut thread_created = false;

    for event in &snapshot.events {
        match &event.payload {
            ThreadEventPayload::ThreadCreated => thread_created = true,
            ThreadEventPayload::ModelRuntimeResolved { .. } => {}
            ThreadEventPayload::InputSubmitted {
                turn_id,
                client_message_id,
                text,
            } => {
                if let Some(key) = client_message_id {
                    // First acceptance wins: a replay must get the identifiers
                    // the client was originally given.
                    accepted.entry(key.clone()).or_insert(Accepted {
                        turn_id: *turn_id,
                        event_id: event.event_id,
                    });
                }
                if !states.iter().any(|(id, _)| id == turn_id) {
                    states.push((*turn_id, TurnState::Queued));
                    submitted.push((
                        *turn_id,
                        Submission {
                            text: text.clone(),
                            client_message_id: client_message_id.clone(),
                            // The log does not carry an origin, and does not
                            // need to: a queued input is replayed straight into
                            // the actor's queue, which never rearms the wake
                            // budget, and a resume starts on a full one anyway
                            // (US-143 AC6).
                            origin: InputOrigin::Human,
                        },
                    ));
                }
            }
            ThreadEventPayload::TurnStateChanged {
                turn_id,
                to,
                context,
                ..
            } => {
                if let Some(captured) = context {
                    turn_context = Some(captured.clone());
                }
                match states.iter_mut().find(|(id, _)| id == turn_id) {
                    Some(slot) => slot.1 = *to,
                    None => states.push((*turn_id, *to)),
                }
            }
            ThreadEventPayload::AgentLinked {
                agent_id,
                name,
                child_thread_id,
                task,
                authority,
            } => agents.push(AgentRecord {
                agent_id: *agent_id,
                // A log written before v2 named its children keeps the opaque
                // handle as its name: explicit legacy value, no rewrite.
                name: name
                    .clone()
                    .unwrap_or_else(|| crate::agent::legacy_name(*agent_id)),
                thread_id: *child_thread_id,
                task: task.clone(),
                state: AgentState::Running,
                authority: authority.clone(),
                started_at_ms: event.at_ms,
                ended_at_ms: None,
                cause: None,
            }),
            ThreadEventPayload::AgentStateChanged {
                agent_id,
                to,
                cause,
            } => {
                if let Some(record) = agents.iter_mut().find(|r| r.agent_id == *agent_id) {
                    record.state = *to;
                    record.cause = cause.clone();
                    record.ended_at_ms = (!to.is_active()).then_some(event.at_ms);
                }
            }
            ThreadEventPayload::AgentMessageQueued {
                agent_id,
                message_id,
                text,
            } => {
                if !agent_messages
                    .iter()
                    .any(|queued| queued.message_id == *message_id)
                {
                    agent_messages.push(AgentMessage {
                        agent_id: *agent_id,
                        message_id: message_id.clone(),
                        text: text.clone(),
                    });
                }
            }
            ThreadEventPayload::AgentMessageDelivered { message_id, .. } => {
                agent_messages.retain(|queued| queued.message_id != *message_id);
            }
            ThreadEventPayload::JobRegistered {
                job_id,
                job_kind,
                command,
            } => jobs.push(JobRecord {
                job_id: *job_id,
                kind: *job_kind,
                command: command.clone(),
                status: JobStatus::Running,
                reported: false,
                exit_code: None,
                cause: None,
                started_at_ms: event.at_ms,
                ended_at_ms: None,
            }),
            ThreadEventPayload::JobStateChanged {
                job_id,
                to,
                exit_code,
                cause,
            } => {
                if let Some(record) = jobs.iter_mut().find(|r| r.job_id == *job_id) {
                    record.status = *to;
                    record.exit_code = *exit_code;
                    record.cause = cause.clone();
                    record.ended_at_ms = to.is_terminal().then_some(event.at_ms);
                }
            }
            ThreadEventPayload::JobReported { job_id } => {
                if let Some(record) = jobs.iter_mut().find(|r| r.job_id == *job_id) {
                    record.reported = true;
                }
            }
            ThreadEventPayload::ScheduleCreated {
                schedule_id,
                rule,
                prompt,
                due_at_ms,
            } => schedules.push(ScheduleChange::Created(ScheduleRecord {
                schedule_id: *schedule_id,
                rule: *rule,
                prompt: prompt.clone(),
                due_at_ms: *due_at_ms,
                // Read back from the entry that carries it rather than stored
                // twice, exactly as a job's `started_at_ms` is.
                created_at_ms: event.at_ms,
            })),
            ThreadEventPayload::ScheduleDispatched {
                schedule_id,
                accepted_at_ms,
            } => schedules.push(ScheduleChange::Dispatched {
                schedule_id: *schedule_id,
                accepted_at_ms: *accepted_at_ms,
            }),
            ThreadEventPayload::ScheduleDeleted { schedule_id } => {
                schedules.push(ScheduleChange::Deleted {
                    schedule_id: *schedule_id,
                })
            }
            ThreadEventPayload::Forked { .. } => {}
        }
    }

    let unfinished: Vec<(TurnId, TurnState)> = states
        .iter()
        .filter(|(_, state)| matches!(state, TurnState::Running | TurnState::NeedsInput))
        .copied()
        .collect();
    let queued = states
        .iter()
        .filter(|(_, state)| *state == TurnState::Queued)
        .filter_map(|(turn_id, _)| {
            submitted
                .iter()
                .find(|(submitted_id, _)| submitted_id == turn_id)
                .cloned()
        })
        .collect();
    let turn = states.last().map(|(turn_id, state)| TurnStatus {
        turn_id: *turn_id,
        state: *state,
    });

    let mut messages = snapshot.messages.clone();
    let tool_results = reconciliation_results(&messages);
    let reconciled_calls = tool_results.len();
    messages.extend(tool_results.clone());

    ResumePlan {
        resumed: ResumedThread {
            thread_id,
            messages,
            turn,
            turn_context,
            recovered: Vec::new(),
            reconciled_calls,
            accepted,
            origin: snapshot.origin,
            agents,
            agent_messages,
            jobs,
            schedules: fold_schedules(&schedules, now_ms),
        },
        unfinished,
        queued,
        tool_results,
        thread_created,
        schedule_changes: schedules,
    }
}

fn reconciliation_results(messages: &[Message]) -> Vec<Message> {
    unanswered_tool_calls(messages)
        .into_iter()
        .map(|id| {
            Message::tool_result_with_metadata(
                id,
                INTERRUPTED_TOOL_RESULT,
                true,
                false,
                Some(ToolErrorKind::Semantic),
            )
        })
        .collect()
}

/// Answers every tool call the transcript left pending and returns how many.
///
/// Same repair as the engine's own reconciliation, applied to a transcript no
/// engine owns anymore. Real results are never replaced: `unanswered_tool_calls`
/// only reports what has no answer at all.
pub fn reconcile(messages: &mut Vec<Message>) -> usize {
    let results = reconciliation_results(messages);
    let repaired = results.len();
    messages.extend(results);
    repaired
}

/// Cause recorded on a turn closed by resume.
pub(crate) fn recovery_cause(from: TurnState) -> String {
    format!("interrupted: recovered from `{from}` at resume")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ThreadEvent;
    use crate::id::{EventId, IdGenerator, ScheduleId, SequentialIds};
    use crate::schedule::{ScheduleRule, ScheduleState};

    /// A fixed instant, so nothing below depends on when it runs.
    const NOW: u64 = 1_770_000_000_000;

    fn snapshot(payloads: Vec<ThreadEventPayload>) -> (ThreadSnapshot, ThreadId) {
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let events = payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| ThreadEvent {
                event_id: EventId::generate(&ids),
                thread_id,
                seq: i as u64 + 1,
                at_ms: i as u64,
                payload,
            })
            .collect();
        (
            ThreadSnapshot {
                thread_id: Some(thread_id),
                events,
                ..ThreadSnapshot::default()
            },
            thread_id,
        )
    }

    /// Same as [`snapshot`], with the wall-clock instant of each entry chosen
    /// by the caller. A schedule fold reads `at_ms` as the creation instant, so
    /// a test about reminders cannot use the tiny counter [`snapshot`] stamps.
    fn snapshot_stamped(payloads: Vec<(u64, ThreadEventPayload)>) -> (ThreadSnapshot, ThreadId) {
        let ids = SequentialIds::new();
        let thread_id = ThreadId::generate(&ids);
        let events = payloads
            .into_iter()
            .enumerate()
            .map(|(i, (at_ms, payload))| ThreadEvent {
                event_id: EventId::generate(&ids),
                thread_id,
                seq: i as u64 + 1,
                at_ms,
                payload,
            })
            .collect();
        (
            ThreadSnapshot {
                thread_id: Some(thread_id),
                events,
                ..ThreadSnapshot::default()
            },
            thread_id,
        )
    }

    fn turn(ids: &dyn IdGenerator) -> TurnId {
        TurnId::generate(ids)
    }

    #[test]
    fn a_turn_left_running_is_reported_unfinished_once() {
        let ids = SequentialIds::starting_at(500);
        let first = turn(&ids);
        let second = turn(&ids);
        let (snap, thread_id) = snapshot(vec![
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id: first,
                client_message_id: None,
                text: "un".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: first,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: first,
                from: Some(TurnState::Running),
                to: TurnState::Completed,
                cause: None,
                context: None,
            },
            ThreadEventPayload::InputSubmitted {
                turn_id: second,
                client_message_id: None,
                text: "deux".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: second,
                from: Some(TurnState::Queued),
                to: TurnState::Running,
                cause: None,
                context: None,
            },
        ]);

        let plan = plan(thread_id, &snap, NOW);
        assert!(plan.thread_created);
        assert_eq!(plan.unfinished, vec![(second, TurnState::Running)]);
        assert_eq!(
            plan.resumed.turn,
            Some(TurnStatus {
                turn_id: second,
                state: TurnState::Running
            })
        );
    }

    #[test]
    fn a_log_whose_turns_all_ended_has_nothing_to_recover() {
        let ids = SequentialIds::starting_at(700);
        let only = turn(&ids);
        let (snap, thread_id) = snapshot(vec![
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id: only,
                client_message_id: Some("cli-1".into()),
                text: "un".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: only,
                from: Some(TurnState::Queued),
                to: TurnState::Interrupted,
                cause: None,
                context: None,
            },
        ]);

        let plan = plan(thread_id, &snap, NOW);
        assert!(plan.unfinished.is_empty());
        assert_eq!(plan.resumed.accepted.len(), 1);
        assert_eq!(plan.resumed.accepted["cli-1"].turn_id, only);
    }

    #[test]
    fn reconcile_answers_only_the_calls_left_pending() {
        use agent_core::message::ContentBlock;

        let mut messages = vec![
            Message::assistant(vec![
                ContentBlock::tool_use("answered", "bash", serde_json::json!({})),
                ContentBlock::tool_use("orphan", "bash", serde_json::json!({})),
            ]),
            Message::tool_result_with_trust("answered", "ok", false, true),
        ];
        assert_eq!(reconcile(&mut messages), 1);
        assert!(unanswered_tool_calls(&messages).is_empty());
        assert_eq!(
            reconcile(&mut messages),
            0,
            "reconciling twice adds nothing"
        );
    }

    /// US-153: the property this epic exists to hold. Folding is the whole of
    /// what resume does with a reminder: it names the overdue one, and it opens
    /// nothing.
    #[test]
    fn a_resume_folds_and_reports_and_opens_no_turn_of_its_own() {
        let ids = SequentialIds::starting_at(900);
        let overdue = ScheduleId::generate(&ids);
        let later = ScheduleId::generate(&ids);
        let (snap, thread_id) = snapshot_stamped(vec![
            (NOW - 3_600_000, ThreadEventPayload::ThreadCreated),
            (
                NOW - 3_600_000,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: overdue,
                    rule: ScheduleRule::After { seconds: 600 },
                    prompt: "relire le journal".into(),
                    due_at_ms: NOW - 3_000_000,
                },
            ),
            (
                NOW - 3_600_000,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: later,
                    rule: ScheduleRule::At {
                        at_ms: NOW + 60_000,
                    },
                    prompt: "plus tard".into(),
                    due_at_ms: NOW + 60_000,
                },
            ),
        ]);

        let plan = plan(thread_id, &snap, NOW);

        assert_eq!(plan.resumed.schedules.active.len(), 2);
        let due = &plan.resumed.schedules.active[0];
        assert_eq!(due.record.schedule_id, overdue);
        assert_eq!(due.state, ScheduleState::Overdue, "the report names it due");
        assert_eq!(
            plan.resumed.schedules.active[1].state,
            ScheduleState::Scheduled
        );
        // The creation instant comes back from the entry that carries it, and
        // is not stored a second time inside the payload.
        assert_eq!(due.record.created_at_ms, NOW - 3_600_000);

        // What a resume did NOT do: no turn, no submission, no repair.
        assert!(plan.queued.is_empty(), "a reminder queues no input");
        assert!(plan.unfinished.is_empty());
        assert!(plan.tool_results.is_empty());
        assert_eq!(plan.resumed.turn, None, "no turn was opened");
        assert!(plan.resumed.messages.is_empty());
    }

    /// US-153: a dispatched one-shot leaves the fold, a deleted one leaves it,
    /// and a recurring one comes back on the slot its acceptance instant names.
    #[test]
    fn a_dispatched_reminder_does_not_come_back_and_a_recurring_one_moves_on() {
        let ids = SequentialIds::starting_at(910);
        let once = ScheduleId::generate(&ids);
        let series = ScheduleId::generate(&ids);
        let dropped = ScheduleId::generate(&ids);
        let first_at = NOW - 900_000;
        let (snap, thread_id) = snapshot_stamped(vec![
            (first_at, ThreadEventPayload::ThreadCreated),
            (
                first_at,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: once,
                    rule: ScheduleRule::At { at_ms: first_at },
                    prompt: "une fois".into(),
                    due_at_ms: first_at,
                },
            ),
            (
                first_at,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: series,
                    rule: ScheduleRule::Every {
                        first_at_ms: first_at,
                        interval_seconds: 300,
                    },
                    prompt: "toutes les cinq minutes".into(),
                    due_at_ms: first_at,
                },
            ),
            (
                first_at,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: dropped,
                    rule: ScheduleRule::At { at_ms: NOW + 1 },
                    prompt: "annulé".into(),
                    due_at_ms: NOW + 1,
                },
            ),
            (
                first_at,
                ThreadEventPayload::ScheduleDispatched {
                    schedule_id: once,
                    accepted_at_ms: None,
                },
            ),
            (
                first_at,
                ThreadEventPayload::ScheduleDispatched {
                    schedule_id: series,
                    accepted_at_ms: Some(first_at),
                },
            ),
            (
                first_at,
                ThreadEventPayload::ScheduleDeleted {
                    schedule_id: dropped,
                },
            ),
        ]);

        let plan = plan(thread_id, &snap, NOW);

        assert_eq!(plan.resumed.schedules.active.len(), 1);
        let view = &plan.resumed.schedules.active[0];
        assert_eq!(view.record.schedule_id, series);
        assert_eq!(
            view.record.due_at_ms,
            first_at + 300_000,
            "the next slot is recomputed from the acceptance instant, never stored"
        );
        assert_eq!(plan.resumed.schedules.corrupt, 0);
    }

    /// US-153: a log written before this lot holds no schedule entry at all,
    /// so a resume finds no reminder, raises nothing, and writes no migration.
    #[test]
    fn a_log_written_before_the_schedule_entries_resumes_with_no_reminder() {
        let ids = SequentialIds::starting_at(920);
        let only = turn(&ids);
        let (snap, thread_id) = snapshot(vec![
            ThreadEventPayload::ThreadCreated,
            ThreadEventPayload::InputSubmitted {
                turn_id: only,
                client_message_id: None,
                text: "un".into(),
            },
            ThreadEventPayload::TurnStateChanged {
                turn_id: only,
                from: Some(TurnState::Queued),
                to: TurnState::Completed,
                cause: None,
                context: None,
            },
        ]);

        let plan = plan(thread_id, &snap, NOW);
        assert_eq!(plan.resumed.schedules, FoldedSchedules::default());
        assert!(plan.tool_results.is_empty(), "nothing was migrated");
        assert!(plan.unfinished.is_empty());
    }

    /// US-153: an unreadable scheduling record is counted, not fatal, and the
    /// reminders around it keep working. A dispatch naming nothing is exactly
    /// what a truncated or partially written log leaves behind.
    #[test]
    fn an_unreadable_schedule_record_is_counted_and_the_others_still_work() {
        let ids = SequentialIds::starting_at(930);
        let ghost = ScheduleId::generate(&ids);
        let sound = ScheduleId::generate(&ids);
        let (snap, thread_id) = snapshot_stamped(vec![
            (NOW - 600_000, ThreadEventPayload::ThreadCreated),
            (
                NOW - 600_000,
                ThreadEventPayload::ScheduleDispatched {
                    schedule_id: ghost,
                    accepted_at_ms: Some(NOW - 600_000),
                },
            ),
            (
                NOW - 600_000,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id: sound,
                    rule: ScheduleRule::After { seconds: 300 },
                    prompt: "toujours là".into(),
                    due_at_ms: NOW - 300_000,
                },
            ),
        ]);

        let plan = plan(thread_id, &snap, NOW);
        assert_eq!(plan.resumed.schedules.corrupt, 1);
        assert_eq!(plan.resumed.schedules.active.len(), 1);
        assert_eq!(plan.resumed.schedules.active[0].record.schedule_id, sound);
    }

    /// US-153: replanning the same snapshot twice yields the same report and
    /// writes nothing, so a double resume can neither duplicate nor lose a
    /// dispatch. Idempotence is the fold being a pure function of the log.
    #[test]
    fn a_second_consecutive_resume_reports_the_same_thing_and_rewrites_nothing() {
        let ids = SequentialIds::starting_at(940);
        let schedule_id = ScheduleId::generate(&ids);
        let (snap, thread_id) = snapshot_stamped(vec![
            (NOW - 600_000, ThreadEventPayload::ThreadCreated),
            (
                NOW - 600_000,
                ThreadEventPayload::ScheduleCreated {
                    schedule_id,
                    rule: ScheduleRule::After { seconds: 60 },
                    prompt: "deux fois".into(),
                    due_at_ms: NOW - 540_000,
                },
            ),
        ]);

        let first = plan(thread_id, &snap, NOW);
        let second = plan(thread_id, &snap, NOW);
        assert_eq!(first.resumed.schedules, second.resumed.schedules);
        assert!(second.tool_results.is_empty());
        assert!(second.queued.is_empty());
    }
}
