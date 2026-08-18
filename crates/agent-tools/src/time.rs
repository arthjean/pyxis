//! Clock tools: `current_time` and `sleep`. Ported from Codex
//! (`codex-rs/core/src/tools/handlers/current_time.rs`,
//! `.../sleep.rs`), which groups them under a `clock` namespace.
//!
//! Both exist for the same reason: a model that cannot read the wall clock
//! invents timestamps, and a model that cannot wait busy-polls a command whose
//! result it already knows is not ready. Neither reads the workspace, neither
//! mutates anything, so both are read-only, non-sensitive and concurrency-safe.
//!
//! [`MAX_SLEEP`] matches the baseline's twelve hours. The bound is what makes
//! long-horizon work possible at all: watching a CI run to completion is mostly
//! waiting, and a ceiling of minutes turns one pause into a hundred model turns
//! that each cost a round trip to say "not yet". `timeout` outlasts it, so the
//! bound the model reads in the schema is the bound that applies.
//!
//! One deliberate deviation remains: **a sleep does not end early on new
//! input.** Pyxis carries steering through the loop's input queue (US-007),
//! which a tool never sees: `ToolCtx` has no channel to it. What a sleep does
//! honour is cancellation: the future is a node of the run's cancel tree
//! (invariant 13), so an interrupt drops it immediately. A user who wants to
//! steer a sleeping agent interrupts it rather than waiting the pause out.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Longest pause a single call may ask for, twelve hours, as in the baseline
/// (`MAX_SLEEP_DURATION_MS` in `codex-rs/core/src/tools/handlers/sleep.rs`). The
/// Registry timeout has to outlast it and is computed without reading the
/// arguments, hence a fixed bound rather than one derived per call.
pub const MAX_SLEEP: Duration = Duration::from_secs(12 * 60 * 60);
/// Grace added on top of [`MAX_SLEEP`] for the tool's own timeout, so the pause
/// itself is never what trips the pipeline.
const SLEEP_TIMEOUT_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SleepInput {
    /// Pause duration in milliseconds.
    pub duration_ms: u64,
}

/// Pauses the turn. Useful when a command was started in the background and its
/// result is known to need a moment: waiting costs one call, polling costs many.
pub struct Sleep;

#[async_trait]
impl Tool for Sleep {
    type Input = SleepInput;

    fn name(&self) -> &str {
        "sleep"
    }
    fn description(&self) -> String {
        format!(
            "Pause for a given duration, then return the elapsed wall-clock \
             time. Bounded to {} ms per call; ask again to wait longer. Use it \
             when something started elsewhere needs time, never as a substitute \
             for waiting on a session with exec_command.",
            MAX_SLEEP.as_millis()
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "duration_ms": {
                    "type": "integer",
                    "description": format!(
                        "How long to pause, in milliseconds. Between 1 and {}.",
                        MAX_SLEEP.as_millis()
                    ),
                }
            },
            "required": ["duration_ms"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// The elapsed time is produced here, from a clock, not from anything a
    /// third party wrote: nothing untrusted enters the context.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_sleep(input.duration_ms)
    }

    /// Outlasts the longest pause the tool accepts, so the bound the model reads
    /// in the schema is the bound that actually applies.
    fn timeout(&self, _ctx: &ToolCtx) -> Duration {
        MAX_SLEEP + SLEEP_TIMEOUT_GRACE
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let requested = Duration::from_millis(input.duration_ms);
        let started = tokio::time::Instant::now();
        tokio::time::sleep(requested).await;
        let elapsed = started.elapsed();
        Ok(ToolOutput::text(format!(
            "Slept {} ms (requested {} ms).",
            elapsed.as_millis(),
            requested.as_millis()
        )))
    }
}

fn validate_sleep(duration_ms: u64) -> Result<(), ValidationError> {
    if duration_ms == 0 {
        return Err(ValidationError::new(
            "duration_ms must be greater than zero: a zero-length pause is not a wait",
        ));
    }
    let max = MAX_SLEEP.as_millis() as u64;
    if duration_ms > max {
        return Err(ValidationError::new(format!(
            "duration_ms too large: {duration_ms} > {max}; call sleep again to wait longer"
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentTimeInput {}

/// Reads the wall clock in UTC. Without it a model dates its work from its
/// training cutoff, which is wrong in a way nothing downstream can detect.
pub struct CurrentTime;

#[async_trait]
impl Tool for CurrentTime {
    type Input = CurrentTimeInput;

    fn name(&self) -> &str {
        "current_time"
    }
    fn description(&self) -> String {
        "Return the current date and time in UTC, formatted as \
         YYYY-MM-DD HH:MM:SS UTC. Call it rather than assuming today's date."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, _input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ToolError::Io("the system clock is before the Unix epoch".into()))?;
        Ok(ToolOutput::text(format_utc(now.as_secs())))
    }
}

/// Epoch seconds -> `YYYY-MM-DD HH:MM:SS UTC`, without a date crate. The civil
/// date conversion is Howard Hinnant's `civil_from_days`, exact over the whole
/// range a `u64` epoch can express, which is what makes a dependency here
/// unjustifiable for one format string.
fn format_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day). Shifts the era origin to
/// 0000-03-01 so leap years fall on a 400-year cycle with no special case.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_the_unix_epoch() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn a_leap_day_is_not_shifted_by_one() {
        // 2024-02-29T12:34:56Z. A civil conversion that mishandles the 400-year
        // cycle lands on March 1st here.
        assert_eq!(format_utc(1_709_210_096), "2024-02-29 12:34:56 UTC");
    }

    #[test]
    fn a_century_year_that_is_not_a_leap_year_is_handled() {
        // 2100 is divisible by 4 but not a leap year: 2100-03-01T00:00:00Z.
        assert_eq!(format_utc(4_107_542_400), "2100-03-01 00:00:00 UTC");
    }

    #[test]
    fn zero_and_oversized_sleeps_are_refused_with_the_bound_named() {
        assert!(validate_sleep(0).is_err());
        let err = validate_sleep(MAX_SLEEP.as_millis() as u64 + 1)
            .expect_err("a pause longer than the bound must be refused");
        assert!(
            err.to_string().contains("call sleep again"),
            "the refusal must tell the model what to do instead: {err}"
        );
        assert!(validate_sleep(1).is_ok());
    }

    #[tokio::test]
    async fn a_sleep_reports_the_elapsed_time() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = Sleep
            .call(SleepInput { duration_ms: 5 }, &ctx)
            .await
            .expect("a bounded sleep must succeed");
        assert!(out.content.contains("requested 5 ms"), "{}", out.content);
    }

    /// The bound is a product decision, not an implementation detail: it is what
    /// decides whether an agent can hold a long-horizon task (watching a CI run,
    /// a deploy) in ONE pause instead of a hundred round trips. Twelve hours is
    /// the baseline's own `MAX_SLEEP_DURATION_MS`.
    #[test]
    fn the_longest_pause_is_the_twelve_hours_of_the_baseline() {
        assert_eq!(MAX_SLEEP.as_millis(), 12 * 60 * 60 * 1000);
        assert!(validate_sleep(12 * 60 * 60 * 1000).is_ok());
    }

    #[test]
    fn the_registry_timeout_outlasts_the_longest_accepted_pause() {
        // Otherwise the bound advertised in the schema would be a lie: the
        // pipeline would kill the call before the tool returns.
        let ctx = ToolCtx::new(std::env::temp_dir());
        assert!(Sleep.timeout(&ctx) > MAX_SLEEP);
    }
}
