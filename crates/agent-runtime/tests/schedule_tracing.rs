//! What the fold SAYS when it refuses a record (US-149, AC6).
//!
//! Its own test binary, for the reason `tests/observability.rs` states at
//! length: `tracing` caches callsite interest globally, so a test that reaches
//! `fold_schedules` without a subscriber installed would resolve the callsite
//! to "nobody is listening" and poison the assertion below for every test that
//! runs after it in the same process.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use agent_runtime::id::SequentialIds;
use agent_runtime::schedule::{
    ScheduleChange, ScheduleId, ScheduleRecord, ScheduleRule, fold_schedules,
};

const NOW: u64 = 1_770_000_000_000;

struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // A poisoned lock is not a reason to lose a trace: the buffer is a
        // `Vec<u8>` and no partial write can leave it inconsistent.
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A record the fold cannot believe is NAMED in a `warn` trace, identifier and
/// reason included. Without it a corrupt count is a number nobody can act on.
#[test]
fn a_refused_record_is_named_in_a_warn_trace() {
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || SharedBuffer(Arc::clone(&sink)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let ids = SequentialIds::new();
    let alive = ScheduleRecord::create(
        ScheduleId::generate(&ids),
        ScheduleRule::After { seconds: 60 },
        "vivant",
        NOW,
    )
    .unwrap();
    // Created nowhere in this sequence: the log lost the line that minted it.
    let ghost = ScheduleId::generate(&ids);

    let folded = fold_schedules(
        &[
            ScheduleChange::Created(alive.clone()),
            ScheduleChange::Deleted { schedule_id: ghost },
        ],
        NOW,
    );
    assert_eq!(folded.corrupt, 1);
    assert_eq!(folded.len(), 1, "the fold kept going");

    let mut writer = SharedBuffer(Arc::clone(&buffer));
    writer.flush().unwrap();
    let captured = {
        let guard = buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    };

    assert!(
        captured.contains("WARN"),
        "the refusal is reported at warn level: {captured}"
    );
    assert!(
        captured.contains("pyxis::runtime"),
        "the refusal is filed under the runtime target: {captured}"
    );
    assert!(
        captured.contains(&ghost.to_string()),
        "the refusal names the reminder it cost: {captured}"
    );
    assert!(
        captured.contains("corrupt schedule record ignored"),
        "the refusal says what happened: {captured}"
    );
    assert!(
        !captured.contains(&alive.schedule_id.to_string()),
        "the reminders around it are not implicated: {captured}"
    );
}
