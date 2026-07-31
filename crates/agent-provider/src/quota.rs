//! Subscription quota headers served by the ChatGPT/Codex backend (US-003).
//!
//! **Evidence that the backend serves this state.** No live request was issued
//! during this story. The header family is the one the Codex CLI reads on the
//! SAME backend: `codex-rs/codex-api/src/rate_limits.rs` builds the names
//! `x-codex-primary-used-percent`, `x-codex-primary-window-minutes` and
//! `x-codex-primary-reset-at` (plus their `secondary` counterparts), and
//! `codex-rs/protocol/src/protocol.rs` documents `resets_at` as "Unix timestamp
//! (seconds since epoch) when the window resets". That client is our reference
//! implementation for this API, so the names and units are read from it rather
//! than guessed. Consequence, and this is the honest limit of the evidence: if
//! the backend stopped serving these headers, nothing here would break, the
//! snapshot would simply stay empty and no indicator would be displayed
//! (US-003 AC5).
//!
//! Codex also parses a `credits` family and several metered limit ids. They are
//! left out: this story exposes consumption and reset, nothing else.

use agent_core::quota::{QuotaCredits, QuotaReachedKind, QuotaSnapshot, QuotaWindow};
use reqwest::header::HeaderMap;

/// Reads the quota state from a response. Returns `None` as soon as nothing
/// usable is served, so that a client never has to distinguish "empty" from
/// "absent".
///
/// The credit and cause header names come from the same reference client:
/// `codex-rs/codex-api/src/rate_limits.rs:217` for `x-codex-credits-*` and
/// `:186` for `x-codex-rate-limit-reached-type`. The plan is NOT served by a
/// header on this backend, which is why it is left to the streamed event.
pub fn parse_quota_headers(headers: &HeaderMap) -> Option<QuotaSnapshot> {
    let snapshot = QuotaSnapshot {
        primary: parse_window(headers, "primary"),
        secondary: parse_window(headers, "secondary"),
        credits: parse_credits(headers),
        plan: None,
        reached: header_str(headers, "x-codex-rate-limit-reached-type")
            .and_then(|value| value.parse::<QuotaReachedKind>().ok()),
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

/// Credit balance. The two booleans are what make the block meaningful: without
/// them a lone balance string says nothing about whether the account can spend.
fn parse_credits(headers: &HeaderMap) -> Option<QuotaCredits> {
    Some(QuotaCredits {
        has_credits: header_bool(headers, "x-codex-credits-has-credits")?,
        unlimited: header_bool(headers, "x-codex-credits-unlimited")?,
        balance: header_str(headers, "x-codex-credits-balance").map(str::to_string),
    })
}

/// Both spellings the backend uses for a flag. Anything else is not a boolean
/// and is treated as absent rather than as `false`.
fn header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    let raw = header_str(headers, name)?;
    if raw.eq_ignore_ascii_case("true") || raw == "1" {
        Some(true)
    } else if raw.eq_ignore_ascii_case("false") || raw == "0" {
        Some(false)
    } else {
        None
    }
}

fn parse_window(headers: &HeaderMap, family: &str) -> Option<QuotaWindow> {
    // The consumption is what makes a window meaningful: without it, a duration
    // or a reset instant alone describes nothing displayable.
    let used_percent = header_str(headers, &format!("x-codex-{family}-used-percent"))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))?;
    let window = QuotaWindow {
        used_percent,
        window_minutes: header_str(headers, &format!("x-codex-{family}-window-minutes"))
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|minutes| *minutes > 0),
        resets_at_unix: header_str(headers, &format!("x-codex-{family}-reset-at"))
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|instant| *instant > 0),
    };
    (!window.is_empty()).then_some(window)
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then_some(value)
}

/// Quota state carried by the streamed `codex.rate_limits` event, which is the
/// ONLY source that names the subscription plan (the headers never do). Baseline:
/// `codex-rs/codex-api/src/rate_limits.rs:134`.
///
/// The plan is kept as a free string rather than a closed enum: it is a label we
/// display and never branch on, and a plan the backend adds tomorrow must not
/// make the whole snapshot unreadable.
pub fn parse_quota_event(value: &serde_json::Value) -> Option<QuotaSnapshot> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("codex.rate_limits") {
        return None;
    }
    let windows = value.get("rate_limits");
    let snapshot = QuotaSnapshot {
        primary: windows.and_then(|w| event_window(w.get("primary"))),
        secondary: windows.and_then(|w| event_window(w.get("secondary"))),
        credits: event_credits(value.get("credits")),
        plan: value
            .get("plan_type")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string),
        reached: None,
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

/// One window of the streamed event. Note the field name: the event says
/// `reset_at` where the header family says `reset-at`.
fn event_window(window: Option<&serde_json::Value>) -> Option<QuotaWindow> {
    let window = window?;
    let used_percent = window
        .get("used_percent")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))?;
    let window = QuotaWindow {
        used_percent,
        window_minutes: window
            .get("window_minutes")
            .and_then(serde_json::Value::as_u64)
            .map(|minutes| minutes as u32)
            .filter(|minutes| *minutes > 0),
        resets_at_unix: window
            .get("reset_at")
            .and_then(serde_json::Value::as_i64)
            .filter(|instant| *instant > 0),
    };
    (!window.is_empty()).then_some(window)
}

fn event_credits(credits: Option<&serde_json::Value>) -> Option<QuotaCredits> {
    let credits = credits?;
    Some(QuotaCredits {
        has_credits: credits
            .get("has_credits")
            .and_then(serde_json::Value::as_bool)?,
        unlimited: credits
            .get("unlimited")
            .and_then(serde_json::Value::as_bool)?,
        balance: credits
            .get("balance")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|balance| !balance.is_empty())
            .map(str::to_string),
    })
}

/// Human-readable sentence for a quota refusal (US-003 AC3): names the limit
/// reached and the reset instant when it is known, instead of handing back the
/// raw HTTP body.
pub fn quota_refusal_message(snapshot: Option<&QuotaSnapshot>) -> String {
    // The named cause takes precedence over the generic sentence: "credits
    // depleted" and "rate limit reached" call for different user actions, and
    // waiting for a window reset fixes only one of them.
    let cause = snapshot
        .and_then(|snapshot| snapshot.reached)
        .map_or("Subscription usage limit reached", QuotaReachedKind::label);
    let Some(window) = snapshot.and_then(QuotaSnapshot::most_consumed) else {
        return format!("{cause}.");
    };
    let scope = match window.window_label() {
        Some(label) => format!(" ({label})"),
        None => String::new(),
    };
    match window.resets_at_label() {
        Some(instant) => format!("{cause}{scope}, resets at {instant}."),
        None => format!("{cause}{scope}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                value.parse().expect("header value"),
            );
        }
        map
    }

    #[test]
    fn parses_both_windows() {
        let snapshot = parse_quota_headers(&headers(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1784989920"),
            ("x-codex-secondary-used-percent", "7"),
            ("x-codex-secondary-window-minutes", "10080"),
        ]))
        .expect("quota served");
        let primary = snapshot.primary.expect("primary window");
        assert_eq!(primary.used_percent, 42.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at_unix, Some(1_784_989_920));
        let secondary = snapshot.secondary.expect("secondary window");
        assert_eq!(secondary.used_percent, 7.0);
        assert_eq!(secondary.resets_at_unix, None);
    }

    /// AC5: nothing served -> nothing to display, and the client is not asked to
    /// tell an empty snapshot from an absent one.
    #[test]
    fn absent_headers_yield_nothing() {
        assert!(parse_quota_headers(&HeaderMap::new()).is_none());
        assert!(
            parse_quota_headers(&headers(&[("x-codex-primary-window-minutes", "300")])).is_none(),
            "une durée sans consommation ne fait pas une fenêtre"
        );
    }

    /// AC4: a malformed or out-of-range state is ignored instead of failing the
    /// turn or displaying something misleading.
    #[test]
    fn malformed_values_are_ignored() {
        assert!(
            parse_quota_headers(&headers(&[(
                "x-codex-primary-used-percent",
                "not-a-number"
            )]))
            .is_none()
        );
        assert!(
            parse_quota_headers(&headers(&[("x-codex-primary-used-percent", "180")])).is_none(),
            "un pourcentage hors bornes n'est pas une mesure"
        );
        let partial = parse_quota_headers(&headers(&[
            ("x-codex-primary-used-percent", "80"),
            ("x-codex-primary-window-minutes", "oops"),
            ("x-codex-primary-reset-at", "-1"),
        ]))
        .expect("la consommation reste exploitable");
        let primary = partial.primary.expect("primary window");
        assert_eq!(primary.used_percent, 80.0);
        assert_eq!(primary.window_minutes, None);
        assert_eq!(primary.resets_at_unix, None);
    }

    #[test]
    fn refusal_message_names_limit_and_reset() {
        let snapshot = QuotaSnapshot {
            primary: Some(QuotaWindow {
                used_percent: 100.0,
                window_minutes: Some(10_080),
                resets_at_unix: Some(1_784_989_920),
            }),
            ..QuotaSnapshot::default()
        };
        assert_eq!(
            quota_refusal_message(Some(&snapshot)),
            "Subscription usage limit reached (1-week window), resets at 2026-07-25 14:32 UTC."
        );
        assert_eq!(
            quota_refusal_message(None),
            "Subscription usage limit reached."
        );
    }
}
