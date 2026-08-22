//! The scheduling domain seen from outside its crate (EP-046).
//!
//! Everything below goes through the public surface of `agent-runtime`, which
//! is the only entry point this epic has: EP-046 ships the pure half, and the
//! durable variants, the timer arm and the three tools that reach it land in
//! EP-047, EP-048 and EP-049.
//!
//! Not one assertion here reads a clock, spawns a task or opens a store. Every
//! instant is a literal, which is what makes the whole file replayable and what
//! keeps its wall-clock cost at zero (NFR "temps mur ajouté à `just test`").
//! The `let ... else { panic!() }` arms below are how a test says "this
//! decision was not the shape I asked for": the binding is what the assertions
//! that follow read, so there is nothing to return and nothing to `expect` on.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::time::Instant;

use agent_runtime::id::{IdGenerator, SequentialIds};
use agent_runtime::schedule::{
    DueDecision, FoldedSchedules, MAX_ACTIVE_SCHEDULES, MAX_SCHEDULE_AT_MS,
    MAX_SCHEDULE_PROMPT_CHARS, MIN_EVERY_INTERVAL_SECONDS, ScheduleChange, ScheduleDelivery,
    ScheduleError, ScheduleId, ScheduleRecord, ScheduleRule, ScheduleState, ScheduleView,
    due_decision, fold_schedules, resolve_every_occurrence,
};

/// A fixed instant, so nothing below depends on when it runs.
const NOW: u64 = 1_770_000_000_000;
const SECOND: u64 = 1_000;
const HOUR: u64 = 3_600 * SECOND;
const DAY: u64 = 24 * HOUR;

fn ids() -> SequentialIds {
    SequentialIds::new()
}

fn schedule_id(ids: &dyn IdGenerator) -> ScheduleId {
    ScheduleId::generate(ids)
}

/// A record built at [`NOW`], refusing to hide a validation failure.
fn record(ids: &dyn IdGenerator, rule: ScheduleRule, prompt: &str) -> ScheduleRecord {
    ScheduleRecord::create(schedule_id(ids), rule, prompt, NOW).expect("the rule is admissible")
}

fn after(seconds: u64) -> ScheduleRule {
    ScheduleRule::After { seconds }
}

fn every(interval_seconds: u64, first_in: u64) -> ScheduleRule {
    ScheduleRule::Every {
        first_at_ms: NOW + first_in,
        interval_seconds,
    }
}

fn created(record: &ScheduleRecord) -> ScheduleChange {
    ScheduleChange::Created(record.clone())
}

fn one_shot_dispatch(record: &ScheduleRecord) -> ScheduleChange {
    ScheduleChange::Dispatched {
        schedule_id: record.schedule_id,
        accepted_at_ms: None,
    }
}

fn recurring_dispatch(record: &ScheduleRecord, accepted_at_ms: u64) -> ScheduleChange {
    ScheduleChange::Dispatched {
        schedule_id: record.schedule_id,
        accepted_at_ms: Some(accepted_at_ms),
    }
}

fn deleted(record: &ScheduleRecord) -> ScheduleChange {
    ScheduleChange::Deleted {
        schedule_id: record.schedule_id,
    }
}

fn due_of(folded: &FoldedSchedules, record: &ScheduleRecord) -> u64 {
    folded
        .get(record.schedule_id)
        .expect("the reminder is active")
        .record
        .due_at_ms
}

// US-148: the closed vocabulary.

/// AC2, AC3, AC4: the three rules, the two states and the nine errors are all
/// the vocabulary there is, and a `match` without a catch-all arm is what
/// proves it. Adding a variant anywhere breaks this file at COMPILE time, which
/// is the whole point of closing the enums.
#[test]
fn the_vocabulary_of_a_reminder_is_closed_and_matched_exhaustively() {
    let ids = ids();
    for rule in [
        after(60),
        ScheduleRule::At { at_ms: NOW + HOUR },
        every(MIN_EVERY_INTERVAL_SECONDS, HOUR),
    ] {
        let named = match rule {
            ScheduleRule::After { seconds } => {
                assert_eq!(seconds, 60);
                "after"
            }
            ScheduleRule::At { at_ms } => {
                assert_eq!(at_ms, NOW + HOUR);
                "at"
            }
            ScheduleRule::Every {
                first_at_ms,
                interval_seconds,
            } => {
                assert_eq!(first_at_ms, NOW + HOUR);
                assert_eq!(interval_seconds, MIN_EVERY_INTERVAL_SECONDS);
                "every"
            }
        };
        assert_eq!(rule.selector(), named);
    }

    for state in [ScheduleState::Scheduled, ScheduleState::Overdue] {
        let named = match state {
            ScheduleState::Scheduled => "scheduled",
            ScheduleState::Overdue => "overdue",
        };
        assert_eq!(state.as_str(), named);
    }

    let errors = [
        ScheduleError::InvalidPrompt { chars: 0 },
        ScheduleError::InvalidSelector { given: 2 },
        ScheduleError::InvalidRule {
            reason: "une raison".into(),
        },
        ScheduleError::InvalidTimeZone {
            zone: "Europe/Paris_2".into(),
        },
        ScheduleError::NotFuture {
            at_ms: NOW,
            now_ms: NOW,
        },
        ScheduleError::TimeOutOfRange,
        ScheduleError::FrequencyTooHigh { seconds: 299 },
        ScheduleError::CorruptScheduleLog {
            reason: "une raison".into(),
        },
        ScheduleError::Internal,
    ];
    let codes: Vec<&str> = errors
        .iter()
        .map(|error| match error {
            ScheduleError::InvalidPrompt { .. } => "invalid_prompt",
            ScheduleError::InvalidSelector { .. } => "invalid_selector",
            ScheduleError::InvalidRule { .. } => "invalid_rule",
            ScheduleError::InvalidTimeZone { .. } => "invalid_time_zone",
            ScheduleError::NotFuture { .. } => "not_future",
            ScheduleError::TimeOutOfRange => "time_out_of_range",
            ScheduleError::FrequencyTooHigh { .. } => "frequency_too_high",
            ScheduleError::CorruptScheduleLog { .. } => "corrupt_schedule_log",
            ScheduleError::Internal => "internal_error",
        })
        .collect();
    assert_eq!(codes.len(), 9, "the error vocabulary is closed at nine");
    for (error, code) in errors.iter().zip(&codes) {
        assert_eq!(&error.code(), code);
    }

    // AC4: `internal_error` never carries what went wrong.
    assert_eq!(
        ScheduleError::Internal.to_string(),
        "internal_error: the reminder could not be handled"
    );

    // AC6: one delivery boundary, as a variant and not as a boolean.
    let delivery = ScheduleDelivery::SessionLocal;
    let named = match delivery {
        ScheduleDelivery::SessionLocal => "session-local",
    };
    assert_eq!(delivery.as_str(), named);
    assert_eq!(record(&ids, after(60), "revenir").delivery(), delivery);
}

/// AC2, AC3, AC4, AC5, AC6: the parts of the vocabulary a compiler cannot
/// check are the doc-comments that carry a decision, so they are asserted on
/// the source itself. Each string below answers a question a reader would
/// otherwise have to guess the answer to.
#[test]
fn the_closed_vocabulary_documents_what_a_reader_cannot_infer() {
    let source = include_str!("../src/schedule.rs");
    for anchor in [
        // AC2: `At` says where zoned civil time is decided.
        "US-162",
        // AC3: `Overdue` says what it buys.
        "`Overdue` is what makes a reminder RECOVERABLE",
        // AC4: the tenth code of the harness is named, with the reason it has
        // no counterpart here.
        "persistence_uncertain",
        "sync_data",
        // AC5: each bound says what it protects.
        "is a token pump",
        "a live process and its pipes",
        "pasted document from becoming a permanent line",
        // AC6: the delivery mode is a v1 boundary, not an accident.
        "Fixed v1 delivery boundary",
    ] {
        assert!(
            source.contains(anchor),
            "the schedule module no longer documents `{anchor}`"
        );
    }
}

/// AC5: the three bounds are crate constants with the values the PRD fixes.
#[test]
fn the_three_bounds_are_crate_constants() {
    assert_eq!(MIN_EVERY_INTERVAL_SECONDS, 300);
    assert_eq!(MAX_ACTIVE_SCHEDULES, 16);
    assert_eq!(MAX_SCHEDULE_PROMPT_CHARS, 1_024);
    // 9999-12-31T23:59:59.999Z, the four-digit-year ceiling.
    assert_eq!(MAX_SCHEDULE_AT_MS, 253_402_300_799_999);
}

/// AC7: a record survives its own serialization, which is what makes it
/// durable at all.
#[test]
fn a_record_round_trips_through_json() {
    let ids = ids();
    for rule in [
        after(90),
        ScheduleRule::At { at_ms: NOW + DAY },
        every(600, HOUR),
    ] {
        let original = record(&ids, rule, "relancer la revue du lot 11");
        let line = serde_json::to_string(&original).unwrap();
        assert_eq!(
            serde_json::from_str::<ScheduleRecord>(&line).unwrap(),
            original
        );
        assert!(line.contains(rule.selector()));
    }

    // The view the model reads travels too, state and boundary included.
    let view = ScheduleView {
        record: record(&ids, after(90), "revenir"),
        state: ScheduleState::Overdue,
        delivery: ScheduleDelivery::SessionLocal,
    };
    let line = serde_json::to_string(&view).unwrap();
    assert!(line.contains("\"overdue\""));
    assert!(line.contains("\"session-local\""));
    assert_eq!(serde_json::from_str::<ScheduleView>(&line).unwrap(), view);
}

/// AC8: 299 seconds is refused, and the refusal names the bound it broke.
#[test]
fn an_interval_below_the_bound_is_refused_and_names_it() {
    let error = every(MIN_EVERY_INTERVAL_SECONDS - 1, HOUR)
        .validate()
        .expect_err("299 seconds is below the floor");
    assert_eq!(error.code(), "frequency_too_high");
    assert!(
        error.to_string().contains("300"),
        "the refusal names the bound: {error}"
    );
    assert_eq!(error, ScheduleError::FrequencyTooHigh { seconds: 299 });
    // Exactly the bound is admissible: the floor is inclusive.
    assert!(every(MIN_EVERY_INTERVAL_SECONDS, HOUR).validate().is_ok());
}

/// The single constructor refuses what could never become a usable reminder,
/// and each refusal is a named value rather than a panic (FR-19).
#[test]
fn a_record_refuses_an_empty_prompt_an_oversized_one_and_a_past_target() {
    let ids = ids();
    let id = schedule_id(&ids);

    let empty = ScheduleRecord::create(id, after(60), "   \n ", NOW).expect_err("empty is refused");
    assert_eq!(empty, ScheduleError::InvalidPrompt { chars: 0 });

    let long = "é".repeat(MAX_SCHEDULE_PROMPT_CHARS + 1);
    let oversized = ScheduleRecord::create(id, after(60), &long, NOW)
        .expect_err("an oversized prompt is refused");
    assert_eq!(
        oversized,
        ScheduleError::InvalidPrompt {
            chars: MAX_SCHEDULE_PROMPT_CHARS + 1
        }
    );
    assert!(
        oversized.to_string().contains("1024"),
        "the refusal names the bound: {oversized}"
    );
    // Counted in characters and not in bytes: the same text one character
    // shorter is accepted, though it is twice that many bytes.
    let at_bound = "é".repeat(MAX_SCHEDULE_PROMPT_CHARS);
    assert!(ScheduleRecord::create(id, after(60), &at_bound, NOW).is_ok());

    let past = ScheduleRecord::create(id, ScheduleRule::At { at_ms: NOW - 1 }, "trop tard", NOW)
        .expect_err("a past target is refused");
    assert_eq!(
        past,
        ScheduleError::NotFuture {
            at_ms: NOW - 1,
            now_ms: NOW
        }
    );

    let now_exactly =
        ScheduleRecord::create(id, ScheduleRule::At { at_ms: NOW }, "maintenant", NOW)
            .expect_err("the current instant is not the future");
    assert_eq!(now_exactly.code(), "not_future");

    let far = ScheduleRecord::create(
        id,
        ScheduleRule::At {
            at_ms: MAX_SCHEDULE_AT_MS + 1,
        },
        "an 99999",
        NOW,
    )
    .expect_err("an unrepresentable target is refused");
    assert_eq!(far, ScheduleError::TimeOutOfRange);

    let zero = ScheduleRecord::create(id, after(0), "tout de suite", NOW)
        .expect_err("a delay of zero is not a delay");
    assert_eq!(zero.code(), "invalid_rule");

    // A delay so long it overflows the epoch is refused, not wrapped.
    let overflow = ScheduleRecord::create(id, after(u64::MAX), "jamais", NOW)
        .expect_err("an overflowing delay is refused");
    assert_eq!(overflow, ScheduleError::TimeOutOfRange);
}

/// The prompt a record keeps is the trimmed one, so the log never carries the
/// whitespace a model happened to send.
#[test]
fn a_record_keeps_the_trimmed_prompt_and_its_creation_instant() {
    let ids = ids();
    let record = ScheduleRecord::create(
        schedule_id(&ids),
        after(2 * 3_600),
        "  relance-moi sur la revue  ",
        NOW,
    )
    .unwrap();
    assert_eq!(record.prompt, "relance-moi sur la revue");
    assert_eq!(record.created_at_ms, NOW);
    assert_eq!(record.due_at_ms, NOW + 2 * HOUR);
    assert_eq!(record.state(NOW), ScheduleState::Scheduled);
    assert_eq!(record.state(NOW + 2 * HOUR), ScheduleState::Overdue);
}

// US-149: the fold.

/// AC1, AC2: the fold hands back the active records, their current target and
/// their state, and the instant it reads them against is a parameter. The same
/// sequence at two instants gives the same records and two different states.
#[test]
fn the_fold_returns_active_records_with_a_state_read_at_a_parameter_instant() {
    let ids = ids();
    let soon = record(&ids, after(60), "bientot");
    let later = record(&ids, after(2 * 3_600), "plus tard");
    let log = [created(&soon), created(&later)];

    let before = fold_schedules(&log, NOW);
    assert_eq!(before.len(), 2);
    assert_eq!(before.corrupt, 0);
    assert_eq!(
        before
            .active
            .iter()
            .map(|view| view.state)
            .collect::<Vec<_>>(),
        vec![ScheduleState::Scheduled, ScheduleState::Scheduled]
    );

    let after_first = fold_schedules(&log, NOW + 90 * SECOND);
    assert_eq!(
        after_first
            .active
            .iter()
            .map(|view| view.state)
            .collect::<Vec<_>>(),
        vec![ScheduleState::Overdue, ScheduleState::Scheduled]
    );
    // Only the reading moved: the records are the same and so are the targets.
    assert_eq!(
        before
            .active
            .iter()
            .map(|view| view.record.clone())
            .collect::<Vec<_>>(),
        after_first
            .active
            .iter()
            .map(|view| view.record.clone())
            .collect::<Vec<_>>()
    );
    // Every active reminder carries the v1 delivery boundary.
    assert!(
        before
            .active
            .iter()
            .all(|view| view.delivery == ScheduleDelivery::SessionLocal)
    );
}

/// AC3: a deleted reminder is ABSENT from the fold, not present and flagged.
#[test]
fn a_deleted_reminder_is_absent_from_the_fold() {
    let ids = ids();
    let kept = record(&ids, after(60), "garde");
    let dropped = record(&ids, after(60), "supprime");
    let folded = fold_schedules(&[created(&kept), created(&dropped), deleted(&dropped)], NOW);
    assert_eq!(folded.len(), 1);
    assert_eq!(folded.corrupt, 0);
    assert!(folded.get(dropped.schedule_id).is_none());
    assert_eq!(folded.get(kept.schedule_id).unwrap().record.prompt, "garde");
}

/// AC4: a delivered one-shot no longer exists. Nothing in the fold could
/// deliver it a second time, because there is nothing left to deliver.
#[test]
fn a_dispatched_one_shot_is_absent_from_the_fold() {
    let ids = ids();
    let once = record(&ids, after(60), "une seule fois");
    let folded = fold_schedules(&[created(&once), one_shot_dispatch(&once)], NOW + HOUR);
    assert!(folded.is_empty());
    assert_eq!(folded.corrupt, 0);
    assert_eq!(
        due_decision(&folded, NOW + HOUR),
        DueDecision::Wait { next_at_ms: None }
    );
}

/// AC5: a recurring dispatch realigns the series on the instant it was ACCEPTED
/// at, and not on the target it was aimed at. That is what the acceptance
/// instant is in the durable line for.
#[test]
fn a_recurring_dispatch_recomputes_the_next_target_from_its_acceptance_instant() {
    let ids = ids();
    let interval = 600;
    let recurring = record(
        &ids,
        every(interval, 10 * 60 * SECOND),
        "toutes les dix minutes",
    );
    let first_target = recurring.due_at_ms;

    // Accepted a full interval and a half late: the series realigns on the slot
    // that instant belongs to, which is one interval past the first target.
    let accepted = first_target + interval * SECOND + 300 * SECOND;
    let folded = fold_schedules(
        &[
            created(&recurring),
            recurring_dispatch(&recurring, accepted),
        ],
        accepted,
    );
    assert_eq!(folded.corrupt, 0);
    assert_eq!(
        due_of(&folded, &recurring),
        first_target + 2 * interval * SECOND,
        "the next target is aligned on the anchor, past the accepted slot"
    );
    assert!(due_of(&folded, &recurring) > accepted);

    // Had it been recomputed from the original target it would have landed one
    // interval earlier, in the past.
    assert_ne!(
        due_of(&folded, &recurring),
        first_target + interval * SECOND
    );
}

/// AC6: a record the fold cannot believe costs its own reminder and nothing
/// else. The count says how many, and the thread still opens.
#[test]
fn a_corrupt_record_is_counted_and_skipped_while_the_fold_continues() {
    let ids = ids();
    let first = record(&ids, after(60), "avant");
    let ghost = record(&ids, after(60), "jamais cree");
    let last = record(&ids, after(60), "apres");

    let folded = fold_schedules(
        &[
            created(&first),
            // Never created here: the log was truncated.
            deleted(&ghost),
            created(&last),
        ],
        NOW,
    );
    assert_eq!(folded.corrupt, 1);
    assert_eq!(folded.len(), 2, "the fold kept going past the bad record");
    assert_eq!(
        folded
            .active
            .iter()
            .map(|view| view.record.prompt.clone())
            .collect::<Vec<_>>(),
        vec!["avant".to_string(), "apres".to_string()]
    );

    // A creation reusing an identifier keeps the FIRST record, and the second
    // is what is counted.
    let twin = ScheduleRecord {
        prompt: "un homonyme".into(),
        ..first.clone()
    };
    let reused = fold_schedules(&[created(&first), created(&twin)], NOW);
    assert_eq!(reused.corrupt, 1);
    assert_eq!(reused.len(), 1);
    assert_eq!(
        reused.get(first.schedule_id).unwrap().record.prompt,
        "avant"
    );
}

/// AC7: a dispatch whose creation the log does not carry is a corrupt record
/// like any other. No panic, no `unwrap`, and the reminders around it are
/// untouched.
#[test]
fn a_dispatch_without_a_creation_is_ignored_as_corrupt() {
    let ids = ids();
    let orphan = record(&ids, after(60), "orphelin");
    let alive = record(&ids, after(60), "vivant");

    let folded = fold_schedules(
        &[
            one_shot_dispatch(&orphan),
            created(&alive),
            recurring_dispatch(&orphan, NOW + HOUR),
        ],
        NOW,
    );
    assert_eq!(folded.corrupt, 2);
    assert_eq!(folded.len(), 1);
    assert_eq!(
        folded.get(alive.schedule_id).unwrap().record.prompt,
        "vivant"
    );
}

/// A one-shot dispatch carrying an acceptance instant is malformed, and it
/// still SPENDS the reminder: a dispatch was written, and keeping the record
/// would deliver it twice.
#[test]
fn a_malformed_one_shot_dispatch_is_counted_and_still_spends_the_reminder() {
    let ids = ids();
    let once = record(&ids, after(60), "une seule fois");
    let folded = fold_schedules(
        &[created(&once), recurring_dispatch(&once, NOW + HOUR)],
        NOW + HOUR,
    );
    assert_eq!(folded.corrupt, 1);
    assert!(folded.is_empty(), "a dispatched one-shot is never replayed");
}

/// A recurring dispatch missing its acceptance instant, or carrying one that
/// precedes the active target, advances the series by exactly one step. The
/// reminder is neither lost nor replayed.
#[test]
fn a_malformed_recurring_dispatch_advances_the_series_by_one_step() {
    let ids = ids();
    let interval = 600;
    for change in [
        // No acceptance instant at all.
        ScheduleChange::Dispatched {
            schedule_id: ScheduleId::generate(&ids),
            accepted_at_ms: None,
        },
        // An acceptance instant before the target it claims to settle.
        ScheduleChange::Dispatched {
            schedule_id: ScheduleId::generate(&ids),
            accepted_at_ms: Some(NOW),
        },
    ] {
        let recurring = ScheduleRecord::create(
            change.schedule_id(),
            every(interval, 10 * 60 * SECOND),
            "toutes les dix minutes",
            NOW,
        )
        .unwrap();
        let first_target = recurring.due_at_ms;
        let folded = fold_schedules(&[created(&recurring), change.clone()], NOW);
        assert_eq!(folded.corrupt, 1);
        assert_eq!(
            due_of(&folded, &recurring),
            first_target + interval * SECOND,
            "the series moved the smallest amount that neither loses nor replays"
        );
    }
}

/// AC8: the fold is a function. Two runs over the same sequence at the same
/// instant are indistinguishable.
#[test]
fn folding_the_same_sequence_twice_gives_the_same_state() {
    let ids = ids();
    let one = record(&ids, after(60), "un");
    let two = record(&ids, every(600, HOUR), "deux");
    let three = record(&ids, after(120), "trois");
    let log = [
        created(&one),
        created(&two),
        created(&three),
        deleted(&three),
        one_shot_dispatch(&one),
        recurring_dispatch(&two, NOW + HOUR + 700 * SECOND),
        // One corrupt record, so the counter is part of what is compared.
        deleted(&three),
    ];
    let first = fold_schedules(&log, NOW + 2 * HOUR);
    let second = fold_schedules(&log, NOW + 2 * HOUR);
    assert_eq!(first, second);
    assert_eq!(first.corrupt, 1);
    assert_eq!(first.len(), 1);
}

/// AC9: the fold is linear in the number of durable records.
///
/// Measured at implementation time on the maintainer's machine: 1 000 records
/// in 0.70 ms, 4 000 in 2.94 ms, a ratio of 4.2 for four times the input, which
/// is linear to within the noise of a debug build. The assertion
/// below is deliberately loose, because a shared runner is not a benchmark
/// bench: a quadratic fold would cost sixteen times more, and eight is the
/// midpoint nothing linear ever crosses.
#[test]
fn the_fold_stays_linear_over_a_thousand_records() {
    fn sequence(entries: usize) -> Vec<ScheduleChange> {
        let ids = SequentialIds::new();
        let mut log = Vec::with_capacity(entries);
        // The shape that costs the most: the active set is held AT the bound
        // while the log grows without limit, and every dispatch spends the
        // OLDEST reminder. That is the worst case for `take`, which shifts the
        // vector and repairs the position map on every removal, and it is the
        // only shape in which the fold could turn quadratic. A sequence that
        // spent each reminder on the line after its creation would keep the
        // active set at one and measure nothing.
        let mut live: VecDeque<ScheduleId> = VecDeque::new();
        while log.len() < entries {
            if live.len() < MAX_ACTIVE_SCHEDULES {
                let one_shot = ScheduleRecord::create(
                    ScheduleId::generate(&ids),
                    ScheduleRule::After { seconds: 60 },
                    "rappel",
                    NOW,
                )
                .expect("the rule is admissible");
                live.push_back(one_shot.schedule_id);
                log.push(ScheduleChange::Created(one_shot));
            } else {
                let oldest = live.pop_front().expect("the bound was reached");
                log.push(ScheduleChange::Dispatched {
                    schedule_id: oldest,
                    accepted_at_ms: None,
                });
            }
        }
        log
    }

    fn cost(log: &[ScheduleChange]) -> u128 {
        // The minimum of a few runs: a scheduler hiccup can only make a run
        // slower, never faster.
        (0..5)
            .map(|_| {
                let start = Instant::now();
                let folded = fold_schedules(log, NOW);
                assert_eq!(folded.corrupt, 0);
                start.elapsed().as_nanos()
            })
            .min()
            .unwrap_or_default()
    }

    let small = sequence(1_000);
    let large = sequence(4_000);
    assert_eq!(small.len(), 1_000);
    assert_eq!(large.len(), 4_000);

    let small_ns = cost(&small);
    let large_ns = cost(&large);
    println!("fold of 1000 records: {small_ns} ns; of 4000: {large_ns} ns");

    assert!(
        small_ns < 50_000_000,
        "folding a thousand records took {small_ns} ns, which is past the 50 ms this suite allows"
    );
    assert!(
        large_ns <= small_ns.max(1) * 8,
        "four times the input cost {large_ns} ns against {small_ns} ns, which is not linear"
    );
}

/// A log written before this batch carries no scheduling record at all, and
/// folding it is a fold of nothing rather than an error (edge case #26).
#[test]
fn a_log_without_a_single_scheduling_record_folds_to_an_empty_state() {
    let folded = fold_schedules(&[], NOW);
    assert!(folded.is_empty());
    assert_eq!(folded.corrupt, 0);
    assert_eq!(folded, FoldedSchedules::default());
}

// US-150: the due decision.

/// AC2, AC5, AC6: the decision is closed at three shapes, an empty state arms
/// nothing, and a future target is what the actor waits on.
#[test]
fn a_decision_is_a_wait_when_nothing_is_due_and_carries_the_next_target() {
    let ids = ids();
    assert_eq!(
        due_decision(&fold_schedules(&[], NOW), NOW),
        DueDecision::Wait { next_at_ms: None },
        "nothing active, nothing armed"
    );

    let soon = record(&ids, after(60), "bientot");
    let later = record(&ids, after(3_600), "plus tard");
    let folded = fold_schedules(&[created(&later), created(&soon)], NOW);
    assert_eq!(
        due_decision(&folded, NOW),
        DueDecision::Wait {
            next_at_ms: Some(NOW + 60 * SECOND)
        },
        "the earliest target is the one to wait on, whatever the log order"
    );
}

/// AC3: two one-shots aimed at the same millisecond come back in the order they
/// were created. The order is total, so the decision never depends on a
/// traversal.
#[test]
fn two_one_shots_due_at_the_same_instant_are_settled_in_creation_order() {
    let ids = ids();
    let first = record(&ids, after(60), "le premier cree");
    let second = record(&ids, after(60), "le second cree");
    assert_eq!(first.due_at_ms, second.due_at_ms);

    let folded = fold_schedules(&[created(&first), created(&second)], NOW + 2 * 60 * SECOND);
    let DueDecision::OneShot { record: chosen } = due_decision(&folded, NOW + 2 * 60 * SECOND)
    else {
        panic!("a due one-shot is a one-shot decision");
    };
    assert_eq!(chosen.prompt, "le premier cree");

    // Once the first is spent, the second follows: one due at a time.
    let after_first = fold_schedules(
        &[created(&first), created(&second), one_shot_dispatch(&first)],
        NOW + 2 * 60 * SECOND,
    );
    let DueDecision::OneShot { record: chosen } = due_decision(&after_first, NOW + 2 * 60 * SECOND)
    else {
        panic!("the second one-shot is due in its turn");
    };
    assert_eq!(chosen.prompt, "le second cree");
}

/// AC4: when both forms are due the one-shot wins, and the order is a written
/// decision rather than an accident of the traversal.
#[test]
fn a_due_one_shot_wins_over_a_due_recurring_reminder() {
    let ids = ids();
    // The recurring one is created FIRST and is due EARLIER, so nothing but the
    // rule of the decision can put the one-shot ahead of it.
    let recurring = record(
        &ids,
        every(MIN_EVERY_INTERVAL_SECONDS, 60 * SECOND),
        "recurrent",
    );
    let one_shot = record(&ids, after(120), "ponctuel");
    let folded = fold_schedules(&[created(&recurring), created(&one_shot)], NOW + HOUR);

    let DueDecision::OneShot { record: chosen } = due_decision(&folded, NOW + HOUR) else {
        panic!("a one-shot outranks a recurring reminder");
    };
    assert_eq!(chosen.prompt, "ponctuel");

    // With the one-shot spent, the recurring batch is what is left.
    let spent = fold_schedules(
        &[
            created(&recurring),
            created(&one_shot),
            one_shot_dispatch(&one_shot),
        ],
        NOW + HOUR,
    );
    let DueDecision::Recurring {
        due,
        accepted_at_ms,
    } = due_decision(&spent, NOW + HOUR)
    else {
        panic!("the recurring reminder is due once the one-shot is gone");
    };
    assert_eq!(accepted_at_ms, NOW + HOUR);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].record.prompt, "recurrent");
}

/// Two recurring reminders that come due together are ONE batch settled at ONE
/// instant, which is what keeps a wake from being spent twice (edge case #34).
#[test]
fn recurring_reminders_that_come_due_together_form_one_batch() {
    let ids = ids();
    let left = record(
        &ids,
        every(MIN_EVERY_INTERVAL_SECONDS, 5 * 60 * SECOND),
        "gauche",
    );
    let right = record(&ids, every(600, 10 * 60 * SECOND), "droite");
    let now = NOW + HOUR;
    let folded = fold_schedules(&[created(&left), created(&right)], now);

    let DueDecision::Recurring {
        due,
        accepted_at_ms,
    } = due_decision(&folded, now)
    else {
        panic!("both recurring reminders are due");
    };
    assert_eq!(accepted_at_ms, now);
    assert_eq!(due.len(), 2);
    assert_eq!(
        due.iter()
            .map(|item| item.record.prompt.clone())
            .collect::<Vec<_>>(),
        vec!["gauche".to_string(), "droite".to_string()]
    );
    // Every occurrence of the batch is collapsed past its own backlog.
    for item in &due {
        assert!(item.occurrence_at_ms <= now);
        assert!(item.occurrence_at_ms >= item.record.due_at_ms);
    }
}

/// AC7: an instant EARLIER than every target yields no dispatch. A wall clock
/// stepping backwards therefore costs a rearm and nothing else (edge case #19).
#[test]
fn an_instant_before_every_target_never_settles_anything() {
    let ids = ids();
    let one_shot = record(&ids, after(3_600), "ponctuel");
    let recurring = record(&ids, every(600, HOUR), "recurrent");
    let folded = fold_schedules(&[created(&one_shot), created(&recurring)], NOW);

    // An hour before the reminders were even created.
    let rewound = NOW - HOUR;
    assert_eq!(
        due_decision(&folded, rewound),
        DueDecision::Wait {
            next_at_ms: Some(NOW + HOUR)
        }
    );
    assert_eq!(folded.len(), 2, "nothing was lost to the clock going back");
}

// US-151: the recurrence.

/// AC4, AC7: the property this whole batch exists for.
#[test]
fn a_recurring_reminder_a_day_late_delivers_one_occurrence_not_a_backlog() {
    let ids = ids();
    let interval = MIN_EVERY_INTERVAL_SECONDS;
    let recurring = record(
        &ids,
        every(interval, interval * SECOND),
        "toutes les cinq minutes",
    );
    let first_target = recurring.due_at_ms;

    // The thread was closed for a day: 288 slots of five minutes went by.
    let accepted = first_target + DAY;
    let occurrence = resolve_every_occurrence(&recurring, accepted).unwrap();

    assert_eq!(
        occurrence.occurrence_at_ms, accepted,
        "the retained slot is the last one that had come due"
    );
    let next = occurrence.next_at_ms.expect("the series goes on");
    assert_eq!(next, accepted + interval * SECOND);
    assert!(next > accepted, "the next target is in the future");

    // And the fold agrees: one dispatch, one step, no queue of missed slots.
    let folded = fold_schedules(
        &[
            created(&recurring),
            recurring_dispatch(&recurring, accepted),
        ],
        accepted,
    );
    assert_eq!(folded.len(), 1);
    assert_eq!(due_of(&folded, &recurring), next);
    assert_eq!(
        due_decision(&folded, accepted),
        DueDecision::Wait {
            next_at_ms: Some(next)
        },
        "nothing else is owed for the day that went by"
    );
}

/// AC1, AC2: one occurrence and one next target, computed by a whole number of
/// steps in a single operation. The cost of the call does not depend on how
/// many slots were missed, which is what the assertion on the retained slot
/// really proves: no enumeration could have produced it in one step.
#[test]
fn a_recurring_occurrence_jumps_a_whole_number_of_steps_at_once() {
    let ids = ids();
    let interval = 600;
    let recurring = record(
        &ids,
        every(interval, interval * SECOND),
        "toutes les dix minutes",
    );
    let target = recurring.due_at_ms;

    for (elapsed, expected_steps) in [
        (0, 0),
        (interval * SECOND - 1, 0),
        (interval * SECOND, 1),
        (7 * interval * SECOND + 42, 7),
        (100_000 * interval * SECOND, 100_000),
    ] {
        let occurrence = resolve_every_occurrence(&recurring, target + elapsed).unwrap();
        assert_eq!(
            occurrence.occurrence_at_ms,
            target + expected_steps * interval * SECOND
        );
        assert_eq!(
            occurrence.next_at_ms,
            Some(target + (expected_steps + 1) * interval * SECOND)
        );
    }
}

/// AC3: an acceptance instant that precedes the active target is a broken fold,
/// not a user input. It is named as a corruption and nothing is computed.
#[test]
fn an_acceptance_instant_before_the_active_target_is_refused() {
    let ids = ids();
    let recurring = record(&ids, every(600, HOUR), "recurrent");
    let error = resolve_every_occurrence(&recurring, recurring.due_at_ms - 1)
        .expect_err("an acceptance instant cannot precede the target it settles");
    assert_eq!(error.code(), "corrupt_schedule_log");
    assert!(error.to_string().contains("precede"), "{error}");

    // Asking a one-shot for a recurring occurrence is the same class of fault.
    let one_shot = record(&ids, after(60), "ponctuel");
    let error = resolve_every_occurrence(&one_shot, NOW + HOUR)
        .expect_err("a one-shot has no series to advance");
    assert_eq!(error.code(), "corrupt_schedule_log");
}

/// AC5: a next target past what an instant can represent ENDS the series
/// instead of corrupting it.
#[test]
fn a_series_that_runs_past_the_representable_range_ends_instead_of_wrapping() {
    let ids = ids();
    let interval = 3_600;
    // A first occurrence one interval short of the ceiling: the slot after the
    // next one falls outside the four-digit-year range.
    let last_slot = MAX_SCHEDULE_AT_MS - interval * SECOND / 2;
    let recurring = ScheduleRecord::create(
        schedule_id(&ids),
        ScheduleRule::Every {
            first_at_ms: last_slot,
            interval_seconds: interval,
        },
        "le dernier rappel du millenaire",
        NOW,
    )
    .unwrap();

    let occurrence = resolve_every_occurrence(&recurring, last_slot).unwrap();
    assert_eq!(occurrence.occurrence_at_ms, last_slot);
    assert_eq!(
        occurrence.next_at_ms, None,
        "the series stops rather than targeting an unrepresentable instant"
    );

    // The fold draws the only conclusion left: the reminder is over.
    let folded = fold_schedules(
        &[
            created(&recurring),
            recurring_dispatch(&recurring, last_slot),
        ],
        last_slot,
    );
    assert!(folded.is_empty());
    assert_eq!(folded.corrupt, 0, "an ended series is not a corruption");
}

/// AC6: the arithmetic is checked end to end. `u64::MAX` on either side is a
/// named refusal, never a panic and never a wrap-around.
#[test]
fn the_recurrence_arithmetic_refuses_u64_max_without_panicking() {
    let ids = ids();
    let recurring = record(&ids, every(600, HOUR), "recurrent");

    let error = resolve_every_occurrence(&recurring, u64::MAX)
        .expect_err("an unrepresentable acceptance instant is refused");
    assert_eq!(error.code(), "corrupt_schedule_log");

    assert_eq!(
        resolve_every_occurrence(&recurring, MAX_SCHEDULE_AT_MS)
            .expect("the ceiling itself is representable")
            .next_at_ms,
        None
    );

    // An interval so large it overflows a millisecond count is refused by the
    // CONSTRUCTOR, which is what keeps such a record out of a log in the first
    // place. Without this the rule was admissible, the reminder was created,
    // and it then sat active and overdue forever: nothing could resolve an
    // occurrence for it, so the decision armed no timer and the fold counted no
    // corruption. A reminder nothing can deliver and nothing reports is the
    // shape this batch exists to make impossible.
    let overflowing = ScheduleRule::Every {
        first_at_ms: NOW + HOUR,
        interval_seconds: u64::MAX,
    };
    assert_eq!(
        overflowing
            .validate()
            .expect_err("an interval past the representable range is refused")
            .code(),
        "time_out_of_range"
    );
    assert_eq!(
        ScheduleRecord::create(schedule_id(&ids), overflowing, "absurde", NOW)
            .expect_err("and the single constructor refuses it too")
            .code(),
        "time_out_of_range"
    );
    // An interval of exactly the representable range is the last one admitted:
    // the bound is a ceiling and not a strict one.
    assert!(
        ScheduleRule::Every {
            first_at_ms: NOW + HOUR,
            interval_seconds: MAX_SCHEDULE_AT_MS / 1_000,
        }
        .validate()
        .is_ok()
    );

    // A durable line that carries one anyway, because a log can be hand-edited,
    // is read as the corruption it is.
    let absurd = ScheduleRecord {
        rule: ScheduleRule::Every {
            first_at_ms: NOW + HOUR,
            interval_seconds: u64::MAX,
        },
        ..recurring.clone()
    };
    let error = resolve_every_occurrence(&absurd, NOW + 2 * HOUR)
        .expect_err("an overflowing interval is refused");
    assert_eq!(error.code(), "corrupt_schedule_log");

    let folded = fold_schedules(
        &[
            ScheduleChange::Created(absurd.clone()),
            ScheduleChange::Dispatched {
                schedule_id: absurd.schedule_id,
                accepted_at_ms: Some(NOW + 2 * HOUR),
            },
        ],
        NOW + 2 * HOUR,
    );
    assert_eq!(folded.corrupt, 1);
    assert!(folded.is_empty());
}
