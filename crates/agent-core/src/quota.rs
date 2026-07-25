//! Subscription quota state reported by the backend (US-003).
//!
//! The core neither measures nor decides anything here: it carries what the
//! provider read, so that a client can display a consumption and a reset time
//! without calling the backend itself. Every field is optional because the
//! backend serves them unevenly, and a partial snapshot must stay usable rather
//! than being dropped whole.

use serde::{Deserialize, Serialize};

/// One rolling quota window (a subscription usually exposes a short one and a
/// long one).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Share of the window already consumed, 0 to 100.
    pub used_percent: f64,
    /// Duration of the rolling window, in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
    /// Instant the window resets, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_unix: Option<i64>,
}

impl QuotaWindow {
    /// A window with no usable data at all: neither consumption, nor duration,
    /// nor reset. Such a window is dropped rather than displayed empty.
    pub fn is_empty(&self) -> bool {
        self.used_percent == 0.0 && self.window_minutes.is_none() && self.resets_at_unix.is_none()
    }

    /// Readable name of the rolling period ("5-hour window"). `None` when the
    /// backend does not declare its duration. Hosted here, next to the unit it
    /// describes, because the provider and the frontend both need it and
    /// duplicating it would let the two drift apart.
    pub fn window_label(&self) -> Option<String> {
        let minutes = self.window_minutes?;
        Some(match minutes {
            m if m % 10_080 == 0 => format!("{}-week window", m / 10_080),
            m if m % 1_440 == 0 => format!("{}-day window", m / 1_440),
            m if m % 60 == 0 => format!("{}-hour window", m / 60),
            m => format!("{m}-minute window"),
        })
    }

    /// Reset instant in UTC, when the backend declares one.
    pub fn resets_at_label(&self) -> Option<String> {
        self.resets_at_unix.map(format_unix_utc)
    }
}

/// Quota state of the connected account at a given instant.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// Short window (the one that usually blocks first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<QuotaWindow>,
    /// Long window (weekly or monthly plan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<QuotaWindow>,
}

impl QuotaSnapshot {
    /// True when the backend served nothing usable: the client must then display
    /// no indicator at all rather than an empty one.
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none()
    }

    /// The window closest to exhaustion, which is the one worth naming in a
    /// message.
    pub fn most_consumed(&self) -> Option<QuotaWindow> {
        match (self.primary, self.secondary) {
            (Some(a), Some(b)) if b.used_percent > a.used_percent => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        }
    }
}

/// Formats a Unix instant as `YYYY-MM-DD HH:MM UTC`. Data formatting, not a
/// presentation decision: the same string is needed by the provider (quota
/// refusal message) and by the frontend, and duplicating the calendar algorithm
/// in two crates would be worse than hosting it here. UTC, because the process
/// timezone is not a piece of information the core owns.
pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute) = (rem / 3_600, (rem % 3_600) / 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm, public
/// domain). Exact inverse of the `days_from_civil` used on the parsing side.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants_in_utc() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00 UTC");
        // 2026-07-25T14:32:00Z
        assert_eq!(format_unix_utc(1_784_989_920), "2026-07-25 14:32 UTC");
        // Leap day, to pin the calendar algorithm down.
        assert_eq!(format_unix_utc(1_709_208_000), "2024-02-29 12:00 UTC");
    }

    #[test]
    fn empty_snapshot_is_recognizable() {
        assert!(QuotaSnapshot::default().is_empty());
        let filled = QuotaSnapshot {
            primary: Some(QuotaWindow {
                used_percent: 12.5,
                window_minutes: Some(300),
                resets_at_unix: Some(1_784_989_920),
            }),
            secondary: None,
        };
        assert!(!filled.is_empty());
    }

    #[test]
    fn window_durations_stay_readable() {
        let window = |minutes| QuotaWindow {
            used_percent: 1.0,
            window_minutes: Some(minutes),
            resets_at_unix: None,
        };
        assert_eq!(
            window(45).window_label().as_deref(),
            Some("45-minute window")
        );
        assert_eq!(window(300).window_label().as_deref(), Some("5-hour window"));
        assert_eq!(
            window(1_440).window_label().as_deref(),
            Some("1-day window")
        );
        assert_eq!(
            window(20_160).window_label().as_deref(),
            Some("2-week window")
        );
        assert_eq!(
            QuotaWindow {
                used_percent: 1.0,
                window_minutes: None,
                resets_at_unix: None,
            }
            .window_label(),
            None
        );
    }

    #[test]
    fn most_consumed_picks_the_tightest_window() {
        let window = |pct| {
            Some(QuotaWindow {
                used_percent: pct,
                window_minutes: None,
                resets_at_unix: None,
            })
        };
        let snapshot = QuotaSnapshot {
            primary: window(10.0),
            secondary: window(90.0),
        };
        assert_eq!(
            snapshot.most_consumed().map(|w| w.used_percent),
            Some(90.0),
            "la fenêtre la plus consommée prime"
        );
        let only_primary = QuotaSnapshot {
            primary: window(10.0),
            secondary: None,
        };
        assert_eq!(
            only_primary.most_consumed().map(|w| w.used_percent),
            Some(10.0)
        );
        assert!(QuotaSnapshot::default().most_consumed().is_none());
    }
}
