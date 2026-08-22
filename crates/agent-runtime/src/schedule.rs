//! Pure scheduling domain of a thread (EP-046).
//!
//! A reminder is a durable RECORD and never a process, a timer or a stored
//! state. Everything this module computes is a function of a sequence of
//! durable changes plus one instant passed in as a parameter: no clock is read
//! here, no task is spawned, and nothing is persisted. That is the split the
//! design harness draws between its 807-line `domain.ts` and its 324-line
//! `runtime.ts`, and the one the previous batch already drew between
//! [`crate::jobs`] and [`crate::thread`].
//!
//! Three properties carry the whole feature:
//! - the active state is REBUILT by folding the log ([`fold_schedules`]), never
//!   written down. A resume is therefore identical to a start, and the class of
//!   bug where a cached projection drifts from its log cannot be expressed.
//! - the fold is TOTAL. A record it cannot make sense of is counted and
//!   skipped, never fatal: a thread must stay openable, which is the deliberate
//!   inversion of the harness's `faulted` latch (`runtime.ts:84`), where one
//!   malformed record stops the whole projection.
//! - a late recurring reminder delivers ONE occurrence
//!   ([`resolve_every_occurrence`]). The arithmetic jumps a whole number of
//!   steps at once, so a five-minute reminder on a thread closed for a day
//!   comes back once and not two hundred and eighty-eight times.
//!
//! Every bound below is a crate constant, never a configuration key
//! (invariant 15, ADR-12).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub use crate::id::ScheduleId;

/// Seconds a recurring reminder must leave between two occurrences.
///
/// Five minutes, the value the design harness ships as
/// `MIN_EVERY_INTERVAL_SECONDS` (`packages/schedule/schedule/src/domain.ts:24`),
/// and stricter than the one-minute floor of `cron`. The reason the floor is
/// higher here is that a dispatch costs a FULL model request, history included,
/// where a `cron` tick costs a `fork`. A reminder that fires more often than a
/// human can read it is a token pump, and raising the bound is buying that pump
/// with one's own budget, which is not a setting anyone should be sold.
pub const MIN_EVERY_INTERVAL_SECONDS: u64 = 300;

/// Reminders a thread may hold ACTIVE at once.
///
/// Deliberately four times [`crate::jobs::MAX_ACTIVE_JOBS`] rather than equal
/// to it: a background job pins a live process and its pipes, a reminder pins
/// one line of a log and one comparison per timer segment. The two bounds
/// protect different things, so aligning them would make the cheaper one pay
/// the price of the more expensive. Sixteen is also what the mailbox already
/// bounds pending inputs at ([`crate::thread::MAX_PENDING_INPUTS`]), which is
/// the queue a dispatched reminder actually lands in: a registry larger than
/// that queue could only produce reminders the queue refuses.
pub const MAX_ACTIVE_SCHEDULES: usize = 16;

/// Characters the text of a reminder may carry.
///
/// The text re-enters the thread later, OUTSIDE the conversation that wrote it,
/// as a durable input nothing truncates afterwards. The bound is what keeps a
/// pasted document from becoming a permanent line of the log, and it is counted
/// in characters and not in bytes because it is the model's own budget that the
/// number has to mean something to.
pub const MAX_SCHEDULE_PROMPT_CHARS: usize = 1_024;

/// Largest instant a reminder may target, in epoch milliseconds:
/// `9999-12-31T23:59:59.999Z`.
///
/// The same four-digit-year ceiling the design harness enforces
/// (`MAX_FOUR_DIGIT_YEAR_MS`, `packages/schedule/schedule/src/domain.ts:27`).
/// It is not an arithmetic limit, `u64` milliseconds reach far past it: it is
/// the boundary beyond which an instant stops being renderable as a date a
/// human reads, and a reminder nobody can read the target of is a reminder
/// nobody can cancel on purpose.
pub const MAX_SCHEDULE_AT_MS: u64 = 253_402_300_799_999;

/// How a reminder is asked for. Closed at three forms, so a `match` that forgets
/// one refuses to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "selector", rename_all = "snake_case")]
pub enum ScheduleRule {
    /// A delay from the instant of creation. Pure epoch arithmetic: no calendar,
    /// no time zone, no dependency.
    After {
        /// Strictly positive; a delay of zero is [`ScheduleError::InvalidRule`].
        seconds: u64,
    },
    /// An absolute instant, in epoch milliseconds.
    ///
    /// v1 takes an instant and nothing else. Accepting a CIVIL date in a named
    /// zone (`2026-08-23 09:00` in `Europe/Paris`) is US-162, which is where
    /// the time-zone database, the daylight-saving gap policy and the fold-back
    /// policy are decided, and where [`ScheduleError::InvalidTimeZone`] gets
    /// its producer. The variant does not change when that story lands: a
    /// civil date is resolved to an instant at creation, and the durable record
    /// carries the instant.
    At {
        /// Bounded by [`MAX_SCHEDULE_AT_MS`].
        at_ms: u64,
    },
    /// A fixed rate, anchored on its first occurrence.
    ///
    /// `first_at_ms` never moves: it is the anchor the whole series is aligned
    /// on, and it is what makes a recomputed occurrence land on the same slot
    /// the original series would have. What moves is
    /// [`ScheduleRecord::due_at_ms`], the earliest occurrence not dispatched
    /// yet.
    Every {
        first_at_ms: u64,
        /// At least [`MIN_EVERY_INTERVAL_SECONDS`].
        interval_seconds: u64,
    },
}

impl ScheduleRule {
    /// Selector name, stable across surfaces: it is what a tool argument is
    /// called, what an error message names, and what the durable line tags.
    pub fn selector(self) -> &'static str {
        match self {
            Self::After { .. } => "after",
            Self::At { .. } => "at",
            Self::Every { .. } => "every",
        }
    }

    /// Does this rule come back after it has been delivered?
    pub fn is_recurring(self) -> bool {
        match self {
            Self::Every { .. } => true,
            Self::After { .. } | Self::At { .. } => false,
        }
    }

    /// Refuses a rule that could never produce a usable record.
    ///
    /// Refuses on the RULE alone, before any instant is involved, so the error
    /// a caller reads names what it asked for and not what the clock made of
    /// it. Whether the resulting target is in the future is a separate question
    /// answered by [`Self::first_due_at_ms`].
    pub fn validate(self) -> Result<(), ScheduleError> {
        match self {
            Self::After { seconds } => {
                if seconds == 0 {
                    return Err(ScheduleError::InvalidRule {
                        reason: "after takes a strictly positive number of seconds".into(),
                    });
                }
                Ok(())
            }
            Self::At { at_ms } => representable(at_ms),
            Self::Every {
                first_at_ms,
                interval_seconds,
            } => {
                if interval_seconds < MIN_EVERY_INTERVAL_SECONDS {
                    return Err(ScheduleError::FrequencyTooHigh {
                        seconds: interval_seconds,
                    });
                }
                // The ceiling of an interval, and not only of a target. An
                // interval longer than the whole representable range can never
                // place a second occurrence inside it, so the reminder would be
                // created, come due once, and then sit active forever with no
                // slot to move to. Refusing it here is what keeps
                // [`Self::interval_ms`] total for every rule this function
                // accepted, which is the invariant [`due_decision`] relies on.
                if interval_seconds
                    .checked_mul(1_000)
                    .is_none_or(|ms| ms > MAX_SCHEDULE_AT_MS)
                {
                    return Err(ScheduleError::TimeOutOfRange);
                }
                representable(first_at_ms)
            }
        }
    }

    /// First instant this rule is due at, given the instant it is created at.
    ///
    /// Takes `now_ms` as a PARAMETER and reads no clock: that is what makes a
    /// creation reproducible in a test and identical on a replay.
    pub fn first_due_at_ms(self, now_ms: u64) -> Result<u64, ScheduleError> {
        self.validate()?;
        let target = match self {
            Self::After { seconds } => seconds
                .checked_mul(1_000)
                .and_then(|delay| now_ms.checked_add(delay))
                .ok_or(ScheduleError::TimeOutOfRange)?,
            Self::At { at_ms } => at_ms,
            Self::Every { first_at_ms, .. } => first_at_ms,
        };
        representable(target)?;
        if target <= now_ms {
            return Err(ScheduleError::NotFuture {
                at_ms: target,
                now_ms,
            });
        }
        Ok(target)
    }

    /// Interval between two occurrences, in milliseconds, for a recurring rule.
    ///
    /// `None` for a one-shot, and for a recurring rule [`Self::validate`] would
    /// have refused. A rule that came through [`ScheduleRecord::create`]
    /// therefore always answers `Some`; the remaining `None` is a hand-edited
    /// durable line, which the fold counts as a corruption.
    fn interval_ms(self) -> Option<u64> {
        match self {
            Self::Every {
                interval_seconds, ..
            } => interval_seconds.checked_mul(1_000).filter(|ms| *ms > 0),
            Self::After { .. } | Self::At { .. } => None,
        }
    }
}

/// Refuses an instant past the four-digit-year ceiling.
fn representable(at_ms: u64) -> Result<(), ScheduleError> {
    if at_ms > MAX_SCHEDULE_AT_MS {
        return Err(ScheduleError::TimeOutOfRange);
    }
    Ok(())
}

/// Where a reminder stands relative to the wall clock. Closed at two values.
///
/// `Overdue` is what makes a reminder RECOVERABLE. A reminder whose target has
/// passed and which has not been delivered is not lost and not an error: it is
/// overdue, and it stays overdue across a refused wake budget, a full input
/// queue and a stopped process, until the thread is live enough to take it. A
/// third value for "delivery was attempted" would have to be persisted to mean
/// anything, and a reminder the log says was attempted is a reminder nothing
/// can retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    /// The target is still in the future.
    Scheduled,
    /// The target has passed and the reminder has not been delivered yet.
    Overdue,
}

impl ScheduleState {
    /// State of a target, read against one instant.
    pub fn at(due_at_ms: u64, now_ms: u64) -> Self {
        if due_at_ms <= now_ms {
            Self::Overdue
        } else {
            Self::Scheduled
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Overdue => "overdue",
        }
    }
}

/// How far a reminder may travel to reach its reader.
///
/// One value in v1, on the pattern of the harness's `ScheduleDeliveryMode`
/// (`packages/schedule/schedule/src/types.ts:111`, "Fixed v1 delivery boundary:
/// the original session must be live"). Deliberately an enum and not a `bool`
/// or an absent field: widening the boundary later, to another thread or to a
/// notification outside the terminal, then costs a VARIANT, which a `match`
/// makes every reader account for. A boolean would cost nothing and be silently
/// wrong at every call site that already ignored it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleDelivery {
    /// The thread that created the reminder, and no other. A reminder of
    /// another thread is not merely refused, it is invisible, exactly like a
    /// background job of another thread.
    SessionLocal,
}

impl ScheduleDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionLocal => "session-local",
        }
    }
}

/// One reminder of a thread, as its log describes it.
///
/// `rule` is what was ASKED FOR and never changes; `due_at_ms` is where the
/// series currently stands and moves with every dispatch. Collapsing the two
/// would lose the anchor a recurring reminder realigns on, and would leave the
/// model unable to read back what it actually created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub schedule_id: ScheduleId,
    pub rule: ScheduleRule,
    /// Reminder text, trimmed and bounded at creation.
    pub prompt: String,
    /// Earliest occurrence not dispatched yet.
    pub due_at_ms: u64,
    pub created_at_ms: u64,
}

impl ScheduleRecord {
    /// Validates a reminder and computes its first target, or names the refusal.
    ///
    /// The ONE constructor: it is what guarantees that a record which exists is
    /// a record whose prompt is bounded, whose rule is admissible and whose
    /// target is both representable and in the future. Reads no clock; `now_ms`
    /// is the single sample its caller took.
    pub fn create(
        schedule_id: ScheduleId,
        rule: ScheduleRule,
        prompt: &str,
        now_ms: u64,
    ) -> Result<Self, ScheduleError> {
        let prompt = prompt.trim();
        let chars = prompt.chars().count();
        if chars == 0 || chars > MAX_SCHEDULE_PROMPT_CHARS {
            return Err(ScheduleError::InvalidPrompt { chars });
        }
        let due_at_ms = rule.first_due_at_ms(now_ms)?;
        Ok(Self {
            schedule_id,
            rule,
            prompt: prompt.to_string(),
            due_at_ms,
            created_at_ms: now_ms,
        })
    }

    /// State of this reminder, read against one instant.
    pub fn state(&self, now_ms: u64) -> ScheduleState {
        ScheduleState::at(self.due_at_ms, now_ms)
    }

    /// Delivery boundary of every reminder of this version.
    pub fn delivery(&self) -> ScheduleDelivery {
        ScheduleDelivery::SessionLocal
    }
}

/// Why a reminder operation was refused.
///
/// Closed at nine variants, each carrying the stable code its [`Self::code`]
/// renders. The codes are the harness's own
/// (`packages/schedule/schedule/src/types.ts:125-198`) minus one:
/// `persistence_uncertain` has no counterpart here and never will, because the
/// state it names cannot be reached. It answers "the write may or may not have
/// landed", which exists in a runtime that batches and flushes; `JsonlThreadStore`
/// calls `sync_data` per line and poisons its writer on failure, so a write here
/// has either landed or been refused with its cause, and there is no third
/// answer to encode.
///
/// Errors are VALUES, never panics and never opaque exceptions: a model that
/// reads `frequency_too_high` with the bound in the message can correct its own
/// call, where a stack trace would only tell it to give up. [`Self::Internal`]
/// is the one that says nothing on purpose: its cause goes to `tracing` and
/// never to the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error(
        "invalid_prompt: a reminder needs a prompt of 1 to {MAX_SCHEDULE_PROMPT_CHARS} characters, got {chars}"
    )]
    InvalidPrompt { chars: usize },
    /// Zero selectors, or more than one. Named separately from
    /// [`Self::InvalidRule`] because the fix is different: one is "pick a
    /// selector", the other is "the selector you picked cannot work".
    #[error("invalid_selector: give exactly one of after, at or every, {given} given")]
    InvalidSelector { given: usize },
    #[error("invalid_rule: {reason}")]
    InvalidRule { reason: String },
    /// Producer lands with US-162, which is where a civil date in a named zone
    /// becomes an instant. The variant exists now so the vocabulary is closed
    /// once and the code set never has to grow under a caller's feet.
    #[error("invalid_time_zone: unknown time zone `{zone}`")]
    InvalidTimeZone { zone: String },
    #[error("not_future: target {at_ms} is not after the current instant {now_ms}")]
    NotFuture { at_ms: u64, now_ms: u64 },
    #[error(
        "time_out_of_range: an instant must stay at or below {MAX_SCHEDULE_AT_MS} epoch ms (9999-12-31T23:59:59.999Z)"
    )]
    TimeOutOfRange,
    #[error(
        "frequency_too_high: every takes at least {MIN_EVERY_INTERVAL_SECONDS} seconds between occurrences, got {seconds}"
    )]
    FrequencyTooHigh { seconds: u64 },
    /// A durable record the fold could not make sense of. Counted and skipped,
    /// never fatal: see [`fold_schedules`].
    #[error("corrupt_schedule_log: {reason}")]
    CorruptScheduleLog { reason: String },
    /// Deliberately says nothing. The cause is traced, never rendered: an
    /// internal failure is not a fact the model can act on, and echoing it back
    /// is how an implementation detail becomes part of a contract.
    #[error("internal_error: the reminder could not be handled")]
    Internal,
}

impl ScheduleError {
    /// Stable machine-readable code. What a tool renders, and what a test
    /// asserts on rather than on a sentence that may be reworded.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPrompt { .. } => "invalid_prompt",
            Self::InvalidSelector { .. } => "invalid_selector",
            Self::InvalidRule { .. } => "invalid_rule",
            Self::InvalidTimeZone { .. } => "invalid_time_zone",
            Self::NotFuture { .. } => "not_future",
            Self::TimeOutOfRange => "time_out_of_range",
            Self::FrequencyTooHigh { .. } => "frequency_too_high",
            Self::CorruptScheduleLog { .. } => "corrupt_schedule_log",
            Self::Internal => "internal_error",
        }
    }

    /// Builds a corruption refusal without repeating the boilerplate.
    fn corrupt(reason: impl Into<String>) -> Self {
        Self::CorruptScheduleLog {
            reason: reason.into(),
        }
    }
}

/// One durable mutation of the scheduling state of a thread.
///
/// The vocabulary the fold consumes. EP-047 maps the three additive variants of
/// [`crate::event::ThreadEventPayload`] onto these three shapes; the domain
/// deliberately does not know what a `ThreadEvent` is, which is what lets every
/// assertion below run without a store, a clock or a tokio runtime.
///
/// The asymmetry of [`Self::Dispatched`] is load-bearing and is taken verbatim
/// from the harness (`packages/schedule/schedule/src/types.ts:93`): a recurring
/// dispatch carries the instant it was ACCEPTED at, a one-shot carries nothing.
/// Without that instant the fold could not recompute which occurrence the
/// series had reached, and the next target would have to be persisted, which is
/// the stored state this module exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleChange {
    /// A reminder came into existence, with the record its creation validated.
    Created(ScheduleRecord),
    /// A reminder was delivered, or a recurring occurrence was.
    Dispatched {
        schedule_id: ScheduleId,
        /// Wall-clock instant the dispatch was decided at. `Some` for a
        /// recurring reminder, `None` for a one-shot.
        accepted_at_ms: Option<u64>,
    },
    /// A reminder was removed before it ever fired again.
    Deleted { schedule_id: ScheduleId },
}

impl ScheduleChange {
    /// Reminder this change is about.
    pub fn schedule_id(&self) -> ScheduleId {
        match self {
            Self::Created(record) => record.schedule_id,
            Self::Dispatched { schedule_id, .. } | Self::Deleted { schedule_id } => *schedule_id,
        }
    }
}

/// One active reminder plus what an instant makes of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleView {
    pub record: ScheduleRecord,
    pub state: ScheduleState,
    pub delivery: ScheduleDelivery,
}

/// The scheduling state of a thread, rebuilt from its log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldedSchedules {
    /// Reminders still active, in the order they were created. Nothing else is
    /// here: a deleted reminder and a delivered one-shot are ABSENT, not
    /// present and flagged, which is what keeps the fold cheap on an old thread
    /// and what keeps a list the model reads free of noise.
    pub active: Vec<ScheduleView>,
    /// Durable records the fold refused to believe. Counted rather than fatal.
    pub corrupt: usize,
}

impl FoldedSchedules {
    /// Reminders currently held, against [`MAX_ACTIVE_SCHEDULES`].
    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// One active reminder by identifier.
    pub fn get(&self, schedule_id: ScheduleId) -> Option<&ScheduleView> {
        self.active
            .iter()
            .find(|view| view.record.schedule_id == schedule_id)
    }
}

/// Rebuilds the scheduling state of a thread from its durable changes.
///
/// A FUNCTION, not a machine: the same sequence and the same instant give the
/// same result every time, which is what makes a resume indistinguishable from
/// a start. `now_ms` is a parameter and no clock is read.
///
/// Total by construction. Every refusal below is counted in
/// [`FoldedSchedules::corrupt`], traced, and skipped, where the harness throws
/// and latches its whole projection `faulted` (`runtime.ts:84`). The inversion
/// is deliberate: a hand-edited or partially written record must cost the
/// reminder it belongs to and never the thread, because a thread nobody can
/// reopen is the one failure with no way back.
///
/// The refusals, and what each one does with the record:
/// - a creation reusing a known identifier is dropped; the first one keeps the
///   name.
/// - a deletion or a dispatch naming no active reminder is dropped. That covers
///   a truncated log, where a dispatch outlives the creation it belongs to.
/// - a one-shot dispatch carrying an acceptance instant still SPENDS the
///   reminder. The line is malformed, but a dispatch was written, and keeping
///   the record would deliver it a second time; delivering twice is the one
///   outcome this module refuses outright.
/// - a recurring dispatch missing its acceptance instant, or carrying one that
///   precedes the active target, advances the series by exactly ONE step from
///   its own target. Dropping the record would lose the reminder and keeping it
///   unchanged would replay the occurrence, so the series moves the smallest
///   amount that does neither.
pub fn fold_schedules<'a, I>(changes: I, now_ms: u64) -> FoldedSchedules
where
    I: IntoIterator<Item = &'a ScheduleChange>,
{
    fn refuse(schedule_id: ScheduleId, reason: &str) {
        tracing::warn!(
            target: "pyxis::runtime",
            schedule_id = %schedule_id,
            reason,
            "corrupt schedule record ignored"
        );
    }

    // Log order is the order reminders were created in, and the tie-break
    // `due_decision` relies on. The map is what keeps a lookup out of the inner
    // loop, so folding a thousand entries stays linear.
    let mut order: Vec<ScheduleRecord> = Vec::new();
    let mut index: HashMap<ScheduleId, usize> = HashMap::new();
    let mut seen: HashSet<ScheduleId> = HashSet::new();
    let mut corrupt = 0usize;

    for change in changes {
        match change {
            ScheduleChange::Created(record) => {
                if !seen.insert(record.schedule_id) {
                    corrupt += 1;
                    refuse(record.schedule_id, "a creation reuses an identifier");
                    continue;
                }
                index.insert(record.schedule_id, order.len());
                order.push(record.clone());
            }
            ScheduleChange::Deleted { schedule_id } => {
                if !take(&mut order, &mut index, *schedule_id) {
                    corrupt += 1;
                    refuse(*schedule_id, "a deletion names no active reminder");
                }
            }
            ScheduleChange::Dispatched {
                schedule_id,
                accepted_at_ms,
            } => {
                let Some(position) = index.get(schedule_id).copied() else {
                    corrupt += 1;
                    refuse(*schedule_id, "a dispatch names no active reminder");
                    continue;
                };
                let due_at_ms = order[position].due_at_ms;
                if !order[position].rule.is_recurring() {
                    if accepted_at_ms.is_some() {
                        corrupt += 1;
                        refuse(
                            *schedule_id,
                            "a one-shot dispatch carries an acceptance instant",
                        );
                    }
                    // Spent either way: a delivered one-shot no longer exists.
                    take(&mut order, &mut index, *schedule_id);
                    continue;
                }
                // A missing or backwards acceptance instant is repaired at the
                // record's own target, which advances the series by exactly one
                // step: dropping the record would lose the reminder, and
                // keeping it unchanged would replay the occurrence.
                let accepted = match accepted_at_ms {
                    None => {
                        corrupt += 1;
                        refuse(
                            *schedule_id,
                            "a recurring dispatch carries no acceptance instant",
                        );
                        due_at_ms
                    }
                    Some(accepted) if *accepted < due_at_ms => {
                        corrupt += 1;
                        refuse(
                            *schedule_id,
                            "a recurring dispatch precedes its active target",
                        );
                        due_at_ms
                    }
                    Some(accepted) => *accepted,
                };
                match resolve_every_occurrence(&order[position], accepted) {
                    Ok(EveryOccurrence {
                        next_at_ms: Some(next),
                        ..
                    }) => order[position].due_at_ms = next,
                    // The series ran past what an instant can represent, or its
                    // interval is unusable. Ending it is the only alternative
                    // to corrupting it; an unusable interval is also counted.
                    Ok(EveryOccurrence {
                        next_at_ms: None, ..
                    }) => {
                        take(&mut order, &mut index, *schedule_id);
                    }
                    Err(err) => {
                        corrupt += 1;
                        refuse(*schedule_id, err.code());
                        take(&mut order, &mut index, *schedule_id);
                    }
                }
            }
        }
    }

    FoldedSchedules {
        active: order
            .into_iter()
            .map(|record| ScheduleView {
                state: record.state(now_ms),
                delivery: record.delivery(),
                record,
            })
            .collect(),
        corrupt,
    }
}

/// Removes one reminder and keeps the position map consistent with the vector.
///
/// `remove` shifts every later element, so the map is repaired in the same
/// breath; a `swap_remove` would be cheaper and would destroy the creation
/// order the tie-break of [`due_decision`] is built on. Removals are rare
/// compared to creations, so the shift is the right side of that trade.
fn take(
    order: &mut Vec<ScheduleRecord>,
    index: &mut HashMap<ScheduleId, usize>,
    schedule_id: ScheduleId,
) -> bool {
    let Some(position) = index.remove(&schedule_id) else {
        return false;
    };
    order.remove(position);
    for slot in index.values_mut() {
        if *slot > position {
            *slot -= 1;
        }
    }
    true
}

/// One occurrence of a recurring reminder, plus where the series goes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EveryOccurrence {
    /// Slot actually retained. Never later than the acceptance instant, and
    /// aligned on the anchor of the series.
    pub occurrence_at_ms: u64,
    /// First strictly future slot, when one is representable. `None` ENDS the
    /// series: a reminder whose next occurrence would fall past
    /// [`MAX_SCHEDULE_AT_MS`] stops rather than wrapping around.
    pub next_at_ms: Option<u64>,
}

/// Resolves one recurring decision without enumerating what was missed.
///
/// The most important arithmetic of this module, and a verbatim port of
/// `resolveEveryOccurrence` (`packages/schedule/schedule/src/domain.ts:519`):
/// the number of whole intervals between the active target and the acceptance
/// instant is computed ONCE and added ONCE. A thread closed for a day with a
/// five-minute reminder therefore comes back with one occurrence, not with two
/// hundred and eighty-eight, and the cost of the computation does not depend on
/// how long the thread was closed.
///
/// Refusing a backlog is a product decision and not an optimization. An
/// occurrence that was missed carries no information a later one does not, and
/// replaying a queue of them would turn reopening a thread into a burst of
/// model requests the human never asked for.
///
/// Every operation is checked. `accepted_at_ms` earlier than the active target
/// is a broken fold and not a user input, so it is
/// [`ScheduleError::CorruptScheduleLog`] and nothing is computed.
pub fn resolve_every_occurrence(
    record: &ScheduleRecord,
    accepted_at_ms: u64,
) -> Result<EveryOccurrence, ScheduleError> {
    let Some(interval_ms) = record.rule.interval_ms() else {
        return Err(ScheduleError::corrupt(
            "a recurring occurrence was asked of a one-shot reminder",
        ));
    };
    if accepted_at_ms > MAX_SCHEDULE_AT_MS {
        return Err(ScheduleError::corrupt(
            "an acceptance instant must stay within the four-digit-year range",
        ));
    }
    if accepted_at_ms < record.due_at_ms {
        return Err(ScheduleError::corrupt(
            "a recurring dispatch cannot precede its active target",
        ));
    }
    let steps = (accepted_at_ms - record.due_at_ms) / interval_ms;
    let occurrence_at_ms = steps
        .checked_mul(interval_ms)
        .and_then(|elapsed| record.due_at_ms.checked_add(elapsed))
        .ok_or_else(|| ScheduleError::corrupt("recurring occurrence arithmetic overflowed"))?;
    let next_at_ms = occurrence_at_ms
        .checked_add(interval_ms)
        .filter(|next| *next <= MAX_SCHEDULE_AT_MS);
    Ok(EveryOccurrence {
        occurrence_at_ms,
        next_at_ms,
    })
}

/// One recurring reminder of a due batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurringDue {
    pub record: ScheduleRecord,
    /// Slot retained for this occurrence, already collapsed past any backlog.
    pub occurrence_at_ms: u64,
}

/// What an instant makes of a folded state. Closed at three shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueDecision {
    /// Exactly one one-shot reminder is to be delivered.
    OneShot { record: ScheduleRecord },
    /// Every recurring reminder that is due, as ONE batch settled at one
    /// instant. A batch and not a queue: two recurring reminders that come due
    /// together are one wake, not two.
    Recurring {
        due: Vec<RecurringDue>,
        /// The instant the whole batch is settled at. Shared, so two reminders
        /// of the same batch cannot realign on two different clocks.
        accepted_at_ms: u64,
    },
    /// Nothing is due. Carries the next target when there is one, which is
    /// exactly what the actor arms its timer on; `None` means nothing is armed.
    Wait { next_at_ms: Option<u64> },
}

/// Chooses what a folded state owes an instant.
///
/// Pure: a folded state and an instant in, a decision out, no clock read. The
/// actor holds no selection logic of its own, which is what keeps the choice
/// testable without a runtime and identical on every replay.
///
/// The order between the two due forms is FIXED and never left to the traversal:
/// a due one-shot wins over a due recurring reminder. A one-shot is spent by its
/// delivery and has no second chance, so postponing it risks losing it to a
/// refused wake budget; postponing a recurring one costs at most a slot, which
/// the last-occurrence rule of [`resolve_every_occurrence`] collapses anyway on
/// the next pass.
///
/// Inside the one-shots the order is total: earliest target first, and creation
/// order breaks a tie. Two reminders aimed at the same millisecond therefore
/// always come back in the order they were asked for.
///
/// An instant EARLIER than every target yields a wait and never a dispatch,
/// which is what makes a wall clock stepping backwards harmless: the reminder
/// stays, the timer is rearmed, and nothing fires on its own.
pub fn due_decision(folded: &FoldedSchedules, now_ms: u64) -> DueDecision {
    // Indices carry creation order through the sort, which `sort_by_key` alone
    // would only preserve by being stable; making it explicit is what the
    // tie-break is asserted on.
    let mut one_shots: Vec<(usize, &ScheduleRecord)> = folded
        .active
        .iter()
        .map(|view| &view.record)
        .enumerate()
        .filter(|(_, record)| !record.rule.is_recurring() && record.due_at_ms <= now_ms)
        .collect();
    one_shots.sort_by_key(|(position, record)| (record.due_at_ms, *position));
    if let Some((_, record)) = one_shots.first() {
        return DueDecision::OneShot {
            record: (*record).clone(),
        };
    }

    let mut recurring: Vec<(usize, &ScheduleRecord)> = folded
        .active
        .iter()
        .map(|view| &view.record)
        .enumerate()
        .filter(|(_, record)| record.rule.is_recurring() && record.due_at_ms <= now_ms)
        .collect();
    recurring.sort_by_key(|(position, record)| (record.due_at_ms, *position));
    if !recurring.is_empty() {
        let due = recurring
            .into_iter()
            .filter_map(|(_, record)| {
                // A record the fold accepted always resolves here; one that
                // does not is a corruption the fold already counted, and it is
                // dropped from the batch rather than allowed to fail the wake.
                resolve_every_occurrence(record, now_ms)
                    .ok()
                    .map(|occurrence| RecurringDue {
                        record: record.clone(),
                        occurrence_at_ms: occurrence.occurrence_at_ms,
                    })
            })
            .collect::<Vec<_>>();
        if !due.is_empty() {
            return DueDecision::Recurring {
                due,
                accepted_at_ms: now_ms,
            };
        }
    }

    DueDecision::Wait {
        next_at_ms: folded
            .active
            .iter()
            .map(|view| view.record.due_at_ms)
            .filter(|due| *due > now_ms)
            .min(),
    }
}
